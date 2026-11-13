//! The pull requests a plan predicts, and how much each one needs a human
//! code review (`docs/quiet-tower.md`, "Predicted PRs and their review
//! score").
//!
//! - **A predicted PR** (`Predicted`) is one entry of a plan: its repo, the
//!   branch it merges into (`dev` first, by the user's release rule), its
//!   title, what it changes, roughly which files, and what the planner says
//!   it does that a person must look at (`migration`, `breaking`,
//!   `escalate`). The planner hands them to the tower (`tower plan prs`);
//!   `check` reads them.
//! - **Its score** (`Score`, `score`): it **needs review** when it adds or
//!   changes a database migration, makes a breaking change (an API,
//!   contract or schema change; something public removed or renamed), or
//!   touches anything on the tower's escalate list (secrets, infrastructure,
//!   auth, deploys, deleting data, money, new dependencies, security). Else
//!   it is **low risk**. What the planner declares counts first; the tower
//!   also reads the files and the words (`hits`), so a migration or a
//!   Terraform file is found even when nobody said so. A word said with
//!   "no", "not" or "without" before it doesn't count ("no migration").
//! - **The real PR** carries the score once it opens: it is matched to its
//!   prediction (`best_match`: the plan launched as its space, the same
//!   repo, the closest title), and scored again on its own title and files
//!   at its head commit (`combine`): what the real PR shows that the plan
//!   didn't (a migration in a "UI" PR) makes it need review.
//! - **Its validation** (`Validation`): every predicted PR lists how its
//!   implementation is validated for sure (`Check`: what it shows, the
//!   exact test, command or live step, and the evidence it produces) and
//!   the edge cases to cover, each tied to the checks that cover it
//!   (`EdgeCase`). The real PR carries them; its session reports each check
//!   (`tower validate`: passed, failed or not run, with its evidence), and
//!   what it hasn't reported when it finishes its turn on the PR is marked
//!   not run (`Validation::finished`).
//! - **Auto-merge** (`auto_path`), behind `[prs] auto_merge_low_risk` (off
//!   unless the user turns it on): a PR into dev that a plan predicted, is
//!   scored low risk, whose planned checks all passed, whose CI checks pass and whose reviewer agreed without
//!   finding anything, is merged by the tower itself (`daemon::automerge`).
//!   Anything that needs review, every merge into main or prod, a PR no plan
//!   predicted, and one the tower couldn't merge stay the user's.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::prs::Pr;

/// A plan predicts this many PRs at most.
pub const MAX_PRS: usize = 30;

/// How a PR stands for review: whether a person must read it, and why.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Score {
    /// A person must review it: a migration, a breaking change, or
    /// something on the escalate list.
    pub needs_review: bool,
    /// What the card says: "Needs review: a database migration
    /// (db/migrations/0042_orders.sql)", "Low risk: no migration, breaking
    /// change, secrets, infrastructure or auth".
    pub line: String,
    /// Each reason found, in order of weight.
    pub reasons: Vec<String>,
    /// What kind each reason is: `migration`, `breaking`, `secrets`,
    /// `infrastructure`, `auth`, `deploy`, `data`, `money`, `dependencies`,
    /// `security`, `escalate`.
    pub flags: Vec<String>,
    /// The plan's prediction it carries (`p3#2`); none when no plan predicted it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planned: Option<String>,
    /// The head commit whose files were read (empty: the prediction alone).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub sha: String,
}

impl Score {
    /// "Needs review" or "Low risk", for a badge.
    pub fn word(&self) -> &'static str {
        if self.needs_review { "Needs review" } else { "Low risk" }
    }

    /// What follows the word on the card: the reasons, or why it is low risk.
    pub fn reason(&self) -> &str {
        self.line.split_once(": ").map(|(_, r)| r).unwrap_or(&self.line)
    }
}

/// One PR a plan says will be opened.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Predicted {
    /// Its number in the plan, from 1.
    pub n: u32,
    /// The repo's folder name (`eyz-customer-support`), as the draft has it.
    pub repo: String,
    /// The branch it merges into: `dev` unless the plan says otherwise.
    pub base: String,
    pub title: String,
    /// What it changes, in a sentence or two.
    pub changes: String,
    /// Roughly which files (paths in the repo, or globs).
    pub files: Vec<String>,
    /// The planner says it adds or changes a database migration.
    pub migration: bool,
    /// What it breaks (an API, contract or schema change; something public
    /// removed or renamed), in the planner's words; empty: nothing.
    pub breaking: String,
    /// Anything else on the escalate list it touches, in the planner's words.
    pub escalate: Vec<String>,
    /// The PRs of the plan (by `n`) it waits for.
    pub after: Vec<u32>,
    /// How its implementation is validated, for sure (at least one).
    pub checks: Vec<Check>,
    /// The edge cases to cover, each tied to the checks that cover it (at least one).
    pub edge_cases: Vec<EdgeCase>,
    /// What outside the changed code uses what it changes, and whether it still works.
    pub dependents: Vec<Uses>,
    /// How the planner checked who depends on it; when nothing does, that it doesn't.
    pub outside: String,
    /// The tower's score of it.
    pub score: Score,
    /// The real PR, once it opened (`owner/name#12`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr: Option<String>,
}

/// Something outside a planned PR that uses what it changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Uses {
    /// What the PR changes that it uses ("the response field translated_text").
    pub what: String,
    /// Who uses it: repo, file, function ("eyzflow-admin-dashboard src/inbox/api.ts loadMessages").
    pub used_by: String,
    /// The change breaks it.
    pub breaks: bool,
}

impl Uses {
    /// "the response field translated_text, used by eyzflow-admin-dashboard src/inbox/api.ts loadMessages".
    pub fn said(&self) -> String {
        format!("{}, used by {}", self.what, self.used_by)
    }
}

impl Predicted {
    /// What the real PR carries: its checks, none reported yet, and its edge cases.
    pub fn validation(&self) -> Validation {
        Validation { checks: self.checks.iter().map(|c| Check { status: PLANNED.into(), proof: String::new(), by: String::new(), at_ms: None, ..c.clone() }).collect(), edge_cases: self.edge_cases.clone() }
    }

    /// What it is scored on: everything the plan says of it.
    pub fn evidence(&self) -> Evidence {
        Evidence {
            title: self.title.clone(),
            changes: self.changes.clone(),
            files: self.files.clone(),
            migration: self.migration,
            breaking: self.breaking.clone(),
            escalate: self.escalate.clone(),
            breaks: self.dependents.iter().filter(|d| d.breaks).map(Uses::said).collect(),
        }
    }
}

/// What a score is made from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evidence {
    pub title: String,
    pub changes: String,
    pub files: Vec<String>,
    pub migration: bool,
    pub breaking: String,
    pub escalate: Vec<String>,
    /// What it breaks for others, and who uses it ("X, used by Y").
    pub breaks: Vec<String>,
}

// ---- how it is validated ----

/// A check not reported yet.
pub const PLANNED: &str = "planned";
pub const PASSED: &str = "passed";
pub const FAILED: &str = "failed";
pub const NOT_RUN: &str = "not_run";

/// One planned check of a PR: how its implementation is validated, for sure.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Check {
    /// `c1`, `c2`, … within its PR.
    pub id: String,
    /// What it shows, concretely ("the migration creates the table and rolls back").
    pub what: String,
    /// How: the exact test to add or run, the command, or the live step
    /// ("cargo test translations::", "open /inbox on dev and toggle a message").
    pub how: String,
    /// The evidence it produces: test output, a screenshot, a log, the deployed state.
    pub evidence: String,
    /// `planned` (not reported yet), `passed`, `failed`, `not_run`.
    pub status: String,
    /// The evidence given with the result: a path, a link, an excerpt of the output, why it wasn't run.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub proof: String,
    /// Who reported it (the session's address); empty when the tower marked it.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_ms: Option<u64>,
}

impl Check {
    pub fn passed(&self) -> bool {
        self.status == PASSED
    }

    /// Its status in words: "passed", "failed", "not run", "not reported yet".
    pub fn status_word(&self) -> &'static str {
        match self.status.as_str() {
            PASSED => "passed",
            FAILED => "failed",
            NOT_RUN => "not run",
            _ => "not reported yet",
        }
    }

    /// "c2 Shows translated messages on dev", its words cut to a line.
    pub fn short(&self) -> String {
        let what: String = self.what.chars().take(60).collect();
        let cut = if self.what.chars().count() > 60 { "…" } else { "" };
        format!("{} {}{cut}", self.id, what.trim_end())
    }
}

/// An edge case to cover, and the checks that cover it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EdgeCase {
    pub case: String,
    /// The ids of the checks that cover it.
    pub checks: Vec<String>,
}

/// A PR's planned validation, and where each check stands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Validation {
    pub checks: Vec<Check>,
    pub edge_cases: Vec<EdgeCase>,
}

impl Validation {
    /// The checks that haven't passed (failed, not run, not reported yet).
    pub fn missing(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.passed()).collect()
    }

    /// Every planned check passed (and there is one).
    pub fn all_passed(&self) -> bool {
        !self.checks.is_empty() && self.checks.iter().all(Check::passed)
    }

    /// A check failed or wasn't run (not only not reported yet).
    pub fn any_bad(&self) -> bool {
        self.checks.iter().any(|c| c.status == FAILED || c.status == NOT_RUN)
    }

    /// The missing ones in a line: "c2 Shows it on dev (failed), c3 Rolls back (not run)".
    pub fn missing_line(&self) -> String {
        self.missing().iter().map(|c| format!("{} ({})", c.short(), c.status_word())).collect::<Vec<_>>().join(", ")
    }

    /// What its card says: "All 3 planned checks passed", or how many
    /// passed and which are missing, and why.
    pub fn line(&self) -> String {
        let n = self.checks.len();
        let passed = n - self.missing().len();
        match () {
            _ if n == 0 => "No planned checks".to_string(),
            _ if passed == n => format!("{}planned check{} passed", if n == 1 { "The " } else if n == 2 { "Both " } else { "All " }, if n == 1 { "" } else { "s" }),
            _ => format!("Checks missing: {} ({passed} of {n} passed)", self.missing_line()),
        }
    }

    /// Its session reports a check (`status`: passed, failed, not-run).
    pub fn report(&mut self, id: &str, status: &str, proof: &str, by: &str, now: u64) -> Result<Check> {
        let status = match status.trim().to_lowercase().replace('-', "_").as_str() {
            "passed" | "pass" | "ok" => PASSED,
            "failed" | "fail" => FAILED,
            "not_run" | "notrun" | "skipped" => NOT_RUN,
            other => bail!("a check is passed, failed or not-run, not {other:?}"),
        };
        let proof = proof.trim();
        if proof.is_empty() {
            bail!("give its evidence: --evidence \"<the test output, a screenshot's path, a log, or why it wasn't run>\"");
        }
        let ids: Vec<String> = self.checks.iter().map(|c| c.id.clone()).collect();
        let Some(c) = self.checks.iter_mut().find(|c| c.id.eq_ignore_ascii_case(id.trim())) else {
            bail!("no planned check {id:?}: its checks are {}", ids.join(", "));
        };
        c.status = status.to_string();
        c.proof = proof.chars().take(2_000).collect();
        c.by = by.to_string();
        c.at_ms = Some(now);
        Ok(c.clone())
    }

    /// Its session finished its turn on the PR: what it didn't report is
    /// not run. The checks it marked so.
    pub fn finished(&mut self, now: u64) -> Vec<Check> {
        let mut marked = vec![];
        for c in self.checks.iter_mut().filter(|c| c.status == PLANNED) {
            c.status = NOT_RUN.into();
            c.proof = "not reported when its session finished its turn on the PR".into();
            c.at_ms = Some(now);
            marked.push(c.clone());
        }
        marked
    }
}

/// What a session is told when it finished its turn on a PR without
/// reporting some of its planned checks.
pub fn unreported_note(pr: &Pr, missing: &[Check], tower: &str) -> String {
    let each: Vec<String> = missing.iter().map(|c| format!("{} {} (how: {}; evidence: {})", c.id, c.what, c.how, c.evidence)).collect();
    format!(
        "[tower] PR #{} ({}) has planned checks you didn't report when your turn ended: {}. Run each, then report it: {tower} validate {} <check> passed --evidence \"<the test output, a screenshot's path, a log>\" (or failed / not-run, with why). Until every planned check passed, the PR is never merged without the user.",
        pr.number,
        pr.repo,
        each.join("; "),
        pr.key()
    )
}

// ---- the kinds of risk ----

/// Each kind, in order of weight, and how a reason of it begins.
const KINDS: &[(&str, &str)] = &[
    ("migration", "a database migration"),
    ("breaking", "a breaking change"),
    ("secrets", "secrets or credentials"),
    ("infrastructure", "infrastructure"),
    ("auth", "auth"),
    ("deploy", "a deploy or release"),
    ("data", "deletes or overwrites data"),
    ("money", "money or billing"),
    ("dependencies", "new dependencies"),
    ("security", "security"),
    ("escalate", "on the escalate list"),
];

fn kind_words(kind: &str) -> &'static str {
    KINDS.iter().find(|(k, _)| *k == kind).map(|(_, w)| *w).unwrap_or("on the escalate list")
}

fn weight(kind: &str) -> usize {
    KINDS.iter().position(|(k, _)| *k == kind).unwrap_or(KINDS.len())
}

/// Words of a text, lower-cased, letters and digits only.
fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect()
}

/// Said as not done: "no", "not", "without" within three words before it.
fn negated(ws: &[String], at: usize) -> bool {
    const NOT: &[&str] = &["no", "not", "without", "nor", "never", "zero", "none", "neither", "isn", "doesn", "don", "won"];
    ws[at.saturating_sub(3)..at].iter().any(|w| NOT.contains(&w.as_str()))
}

/// The first of `phrases` (words in a row) said in `ws`, and not as not done.
fn spoken<'a>(ws: &[String], phrases: &[&'a str]) -> Option<&'a str> {
    for p in phrases {
        let want: Vec<&str> = p.split(' ').collect();
        if want.len() > ws.len() {
            continue;
        }
        for i in 0..=ws.len() - want.len() {
            if want.iter().enumerate().all(|(j, w)| ws[i + j] == *w) && !negated(ws, i) {
                return Some(p);
            }
        }
    }
    None
}

/// Something public removed or renamed: a verb that takes it away, then
/// within four words a thing others use ("renames the total field",
/// "removes the v1 endpoint"). The two words, as found.
fn removes_public(ws: &[String]) -> Option<String> {
    const VERBS: &[&str] = &[
        "remove", "removes", "removed", "removing", "rename", "renames", "renamed", "renaming", "delete", "deletes", "deleted", "deleting", "drop", "drops", "dropped",
        "dropping", "deprecate", "deprecates", "deprecated", "replace", "replaces", "replaced",
    ];
    const PUBLIC: &[&str] = &[
        "api", "apis", "endpoint", "endpoints", "route", "routes", "field", "fields", "column", "columns", "param", "params", "parameter", "parameters", "flag", "flags",
        "export", "exports", "exported", "method", "methods", "function", "functions", "class", "classes", "interface", "interfaces", "type", "types", "event", "events",
        "property", "properties", "prop", "props", "argument", "arguments", "header", "headers", "env", "variable", "variables", "command", "commands", "webhook",
        "webhooks", "contract", "schema", "payload", "response", "request", "table", "tables", "topic", "queue", "url", "urls", "sdk",
    ];
    for (i, w) in ws.iter().enumerate() {
        if !VERBS.contains(&w.as_str()) || negated(ws, i) {
            continue;
        }
        if let Some(n) = ws[i + 1..ws.len().min(i + 5)].iter().find(|n| PUBLIC.contains(&n.as_str())) {
            return Some(format!("{w} … {n}"));
        }
    }
    None
}

/// What the words of a PR say it does that a person must look at: (kind, the words found).
fn word_hits(text: &str) -> Vec<(&'static str, String)> {
    let ws = tokens(text);
    let mut out: Vec<(&'static str, String)> = vec![];
    let mut add = |kind: &'static str, found: Option<String>| {
        if let Some(f) = found
            && !out.iter().any(|(k, _)| *k == kind)
        {
            out.push((kind, f));
        }
    };
    let quoted = |s: Option<&str>| s.map(|s| format!("\u{201c}{s}\u{201d}"));
    add(
        "migration",
        quoted(spoken(
            &ws,
            &[
                "migration", "migrations", "migrate", "migrates", "migrating", "alter table", "add column", "adds a column", "add a column", "new column", "drop column",
                "drop table", "create table", "new table", "rename column", "add index", "adds an index", "backfill", "backfills", "schema change",
            ],
        )),
    );
    add(
        "breaking",
        quoted(spoken(
            &ws,
            &[
                "breaking", "backwards incompatible", "backward incompatible", "incompatible", "api change", "changes the api", "contract change", "changes the contract",
                "changes the schema", "changes the response", "changes the payload", "response shape", "payload shape", "public api",
            ],
        ))
        .or_else(|| removes_public(&ws).map(|s| format!("\u{201c}{s}\u{201d}"))),
    );
    add(
        "secrets",
        quoted(spoken(
            &ws,
            &[
                "secret", "secrets", "credential", "credentials", "password", "passwords", "api key", "api keys", "private key", "access key", "access token", "api token",
                "auth token", "signing key", "encryption key", "client secret", "env file",
            ],
        )),
    );
    add(
        "infrastructure",
        quoted(spoken(
            &ws,
            &[
                "aws", "terraform", "infrastructure", "infra", "dns", "cloudformation", "kubernetes", "k8s", "helm", "dockerfile", "docker compose", "iam", "vpc", "ec2",
                "cloudfront", "route53", "load balancer", "github actions", "ci pipeline", "ci workflow", "cdk", "s3 bucket", "lambda", "serverless", "nginx",
            ],
        )),
    );
    add(
        "auth",
        quoted(spoken(
            &ws,
            &[
                "auth", "authentication", "authorization", "authorisation", "authorize", "login", "logout", "log in", "sign in", "signin", "sign up", "signup", "oauth", "sso",
                "saml", "jwt", "permission", "permissions", "rbac", "access control", "csrf", "cors", "2fa", "mfa", "session cookie",
            ],
        )),
    );
    // A release build or release notes aren't a release.
    let no_builds: Vec<String> = {
        let mut v = ws.clone();
        for i in 0..v.len().saturating_sub(1) {
            if v[i] == "release" && matches!(v[i + 1].as_str(), "notes" | "build" | "builds" | "mode" | "profile") {
                v[i] = String::new();
            }
        }
        v
    };
    add(
        "deploy",
        quoted(spoken(&no_builds, &["deploy", "deploys", "deploying", "deployment", "release", "releases", "releasing", "publish", "publishes", "production", "prod", "go live", "rollout"])),
    );
    add(
        "data",
        quoted(spoken(
            &ws,
            &[
                "delete data", "deletes data", "delete all", "deletes all", "hard delete", "bulk delete", "purge", "purges", "purging", "truncate", "truncates", "wipe",
                "wipes", "overwrite", "overwrites", "destroy", "destroys", "data fix",
            ],
        )),
    );
    add(
        "money",
        quoted(spoken(
            &ws,
            &["billing", "payment", "payments", "stripe", "invoice", "invoices", "pricing", "price change", "subscription", "subscriptions", "refund", "refunds", "paid plan", "paid service"],
        )),
    );
    add("dependencies", quoted(spoken(&ws, &["dependency", "dependencies", "new library", "new package", "new sdk"])));
    add(
        "security",
        quoted(spoken(&ws, &["security", "vulnerability", "vulnerabilities", "xss", "injection", "encryption", "encrypt", "decrypt", "cve", "sanitize", "sanitise"])),
    );
    out
}

/// A path's words: its folders and its name, split at `-`, `_`, `.` and
/// between a small letter and a capital (`AuthProvider` is `auth provider`).
fn path_words(path: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in path.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur).to_lowercase());
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur).to_lowercase());
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur.to_lowercase());
    }
    out
}

/// What kind of file a path is, when a person must look at a change to it.
pub fn file_kind(path: &str) -> Option<&'static str> {
    let p = path.trim().trim_start_matches("./").to_lowercase();
    if p.is_empty() {
        return None;
    }
    let name = p.rsplit('/').next().unwrap_or(&p).to_string();
    let dirs: Vec<&str> = p.split('/').collect();
    let dir = |d: &[&str]| dirs[..dirs.len().saturating_sub(1)].iter().any(|x| d.contains(x));
    let words = path_words(path);
    let word = |w: &[&str]| words.iter().any(|x| w.contains(&x.as_str()));
    let ends = |e: &[&str]| e.iter().any(|x| name.ends_with(x));
    // A database migration, or the schema it changes.
    if dir(&["migrations", "migration", "migrate", "alembic", "flyway", "liquibase"])
        || ends(&[".sql"])
        || name.contains("migration")
        || matches!(name.as_str(), "schema.prisma" | "schema.rb" | "structure.sql")
    {
        return Some("migration");
    }
    // An API's contract.
    if ends(&[".proto", ".graphql", ".gql", ".avsc"]) || name.starts_with("openapi") || name.starts_with("swagger") {
        return Some("breaking");
    }
    // Secrets.
    let example = ["example", "sample", "template", "dist"].iter().any(|x| name.contains(x));
    if (name.starts_with(".env") && !example)
        || ends(&[".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"])
        || name.starts_with("id_rsa")
        || matches!(name.as_str(), ".npmrc" | ".netrc" | ".pypirc")
        || word(&["secret", "secrets", "credential", "credentials"])
    {
        return Some("secrets");
    }
    // Infrastructure, CI and deploys.
    if dir(&["terraform", "infra", "infrastructure", "cdk", "k8s", "kubernetes", "helm", "charts", "ansible", "pulumi", "deploy", "deployment", "deployments", "cloudformation", ".circleci"])
        || p.starts_with(".github/workflows/")
        || ends(&[".tf", ".tfvars", ".hcl"])
        || name.starts_with("dockerfile")
        || matches!(
            name.as_str(),
            "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml" | "serverless.yml" | "serverless.yaml" | "serverless.ts" | "serverless.js"
                | "template.yaml" | "template.yml" | "samconfig.toml" | "vercel.json" | "netlify.toml" | "fly.toml" | "procfile" | "app.yaml" | "jenkinsfile"
                | ".gitlab-ci.yml" | "nginx.conf" | "cdk.json" | "amplify.yml" | "buildspec.yml" | "appspec.yml" | "wrangler.toml"
        )
    {
        return Some("infrastructure");
    }
    // Auth.
    let pairs = words.windows(2).any(|w| matches!((w[0].as_str(), w[1].as_str()), ("sign", "in") | ("sign", "up") | ("log", "in") | ("log", "out")));
    if pairs
        || word(&[
            "auth", "authn", "authz", "authentication", "authorization", "authorize", "login", "logout", "signin", "signup", "oauth", "oauth2", "sso", "saml", "jwt",
            "permission", "permissions", "rbac", "acl", "acls", "passport", "csrf",
        ])
    {
        return Some("auth");
    }
    // Money.
    if word(&["billing", "payment", "payments", "stripe", "invoice", "invoices", "pricing", "subscription", "subscriptions"]) {
        return Some("money");
    }
    // A dependency manifest (a lockfile alone is the same dependencies).
    if matches!(
        name.as_str(),
        "package.json" | "cargo.toml" | "go.mod" | "requirements.txt" | "pyproject.toml" | "pipfile" | "gemfile" | "composer.json" | "build.gradle"
            | "build.gradle.kts" | "pom.xml" | "package.swift" | "podfile" | "pubspec.yaml"
    ) {
        return Some("dependencies");
    }
    None
}

/// Everything found in `e`, one reason per kind (the planner's own word
/// first, then the files, then the words), heaviest first: (kind, reason).
pub fn hits(e: &Evidence) -> Vec<(&'static str, String)> {
    let mut out: Vec<(&'static str, String)> = vec![];
    let mut add = |kind: &'static str, what: String| {
        if !out.iter().any(|(k, _)| *k == kind) {
            out.push((kind, format!("{} ({})", kind_words(kind), what)));
        }
    };
    // The files of each kind: the first, and how many more.
    let mut by_kind: Vec<(&'static str, Vec<&str>)> = vec![];
    for f in e.files.iter().map(|f| f.trim()).filter(|f| !f.is_empty()) {
        if let Some(k) = file_kind(f) {
            match by_kind.iter_mut().find(|(x, _)| *x == k) {
                Some((_, list)) => list.push(f),
                None => by_kind.push((k, vec![f])),
            }
        }
    }
    let files_of = |k: &str| {
        by_kind.iter().find(|(x, _)| *x == k).map(|(_, list)| match list.len() {
            1 => list[0].to_string(),
            n => format!("{} and {} more", list[0], n - 1),
        })
    };
    if e.migration {
        add("migration", files_of("migration").unwrap_or_else(|| "the plan says so".into()));
    }
    // What it breaks: the planner's word, then each dependent it breaks.
    let mut parts: Vec<String> = e.breaking.trim().is_empty().then(Vec::new).unwrap_or_else(|| vec![e.breaking.trim().to_string()]);
    parts.extend(e.breaks.iter().map(|b| b.trim().to_string()).filter(|b| !b.is_empty()));
    if !parts.is_empty() {
        let mut what = parts.iter().take(3).cloned().collect::<Vec<_>>().join("; ");
        if parts.len() > 3 {
            what += &format!("; and {} more", parts.len() - 3);
        }
        add("breaking", what);
    }
    for x in e.escalate.iter().map(|x| x.trim()).filter(|x| !x.is_empty()) {
        let kind = word_hits(x).first().map(|(k, _)| *k).unwrap_or("escalate");
        add(kind, x.to_string());
    }
    for (k, _) in &by_kind {
        let what = files_of(k).unwrap_or_default();
        add(k, if *k == "breaking" { format!("{what}: an API contract or schema") } else { what });
    }
    for (k, words) in word_hits(&format!("{}\n{}", e.title, e.changes)) {
        add(k, words);
    }
    out.sort_by_key(|(k, _)| weight(k));
    out
}

/// What a low-risk PR's card says it isn't.
pub const LOW_LINE: &str = "Low risk: no migration, breaking change, secrets, infrastructure or auth";

/// The score of what `e` says.
pub fn score(e: &Evidence) -> Score {
    let found = hits(e);
    let reasons: Vec<String> = found.iter().map(|(_, r)| r.clone()).collect();
    let flags: Vec<String> = found.iter().map(|(k, _)| k.to_string()).collect();
    let line = if reasons.is_empty() { LOW_LINE.to_string() } else { format!("Needs review: {}", reasons_line(&reasons)) };
    Score { needs_review: !reasons.is_empty(), line, reasons, flags, ..Default::default() }
}

/// The first two reasons, and how many more.
fn reasons_line(reasons: &[String]) -> String {
    let mut line = reasons.iter().take(2).cloned().collect::<Vec<_>>().join("; ");
    if reasons.len() > 2 {
        line += &format!("; and {} more", reasons.len() - 2);
    }
    line
}

/// The score a real PR carries: its prediction's (when a plan predicted
/// it) with what its own title and files at `sha` show added. What the real
/// PR shows and the plan didn't is said so.
pub fn combine(predicted: Option<&Score>, real: &Score, sha: &str) -> Score {
    let mut s = predicted.cloned().unwrap_or_default();
    let mut unplanned: Vec<String> = vec![];
    for (k, r) in real.flags.iter().zip(&real.reasons) {
        if !s.flags.contains(k) {
            s.flags.push(k.clone());
            s.reasons.push(r.clone());
            unplanned.push(r.clone());
        }
    }
    // Heaviest first, as `hits` orders them.
    let mut both: Vec<(String, String)> = s.flags.iter().cloned().zip(s.reasons.iter().cloned()).collect();
    both.sort_by_key(|(k, _)| weight(k));
    (s.flags, s.reasons) = both.into_iter().unzip();
    s.needs_review = !s.reasons.is_empty();
    s.line = match () {
        _ if !s.needs_review => LOW_LINE.to_string(),
        // The plan said low risk: what the real PR shows instead is the news.
        _ if predicted.is_some_and(|p| !p.needs_review) => format!("Needs review: {}, which the plan didn't predict", reasons_line(&unplanned)),
        _ => format!("Needs review: {}", reasons_line(&s.reasons)),
    };
    s.sha = sha.to_string();
    s
}

/// The score of a real PR by its own title and files (`gh pr view --json files`).
pub fn real_score(title: &str, files: &[String]) -> Score {
    score(&Evidence { title: title.into(), files: files.to_vec(), ..Default::default() })
}

/// The file paths of `gh pr view --json files`.
pub fn files_of(v: &Value) -> Vec<String> {
    v["files"].as_array().into_iter().flatten().filter_map(|f| f["path"].as_str().or(f.as_str())).map(String::from).collect()
}

/// What a PR found just now carries from the start (the daemon's scorer):
/// its score, the validation its plan gives it, and who depends on what it changes.
#[derive(Debug, Clone, Default)]
pub struct Scored {
    pub score: Option<Score>,
    pub validation: Option<Validation>,
    pub impact: Option<crate::impact::Impact>,
}

/// Two readings of the same real PR as one score (its files and words,
/// and who depends on what it changes): every reason of both.
pub fn union(a: &Score, b: &Score) -> Score {
    let mut s = a.clone();
    for (k, r) in b.flags.iter().zip(&b.reasons) {
        if !s.flags.contains(k) {
            s.flags.push(k.clone());
            s.reasons.push(r.clone());
        }
    }
    let mut both: Vec<(String, String)> = s.flags.iter().cloned().zip(s.reasons.iter().cloned()).collect();
    both.sort_by_key(|(k, _)| weight(k));
    (s.flags, s.reasons) = both.into_iter().unzip();
    s.needs_review = !s.reasons.is_empty();
    s.line = if s.needs_review { format!("Needs review: {}", reasons_line(&s.reasons)) } else { LOW_LINE.to_string() };
    s
}

// ---- reading the planner's list ----

/// A repo's folder name, from a name or a path.
pub fn folder_name(repo: &str) -> &str {
    repo.trim().trim_end_matches('/').rsplit('/').next().unwrap_or(repo).trim_end_matches(".git")
}

/// A field that is a list of words, or one string of them (comma-separated).
fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Value::String(s) => s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        _ => vec![],
    }
}

/// The predicted PRs the planner wrote (`{"prs": [...]}`, or the list
/// alone), checked against the draft's repos (folders, whole or by name)
/// and scored. Each needs its repo and title; `base` is `dev` when not said.
pub fn check(v: &Value, repos: &[String]) -> Result<Vec<Predicted>> {
    let list = v.get("prs").unwrap_or(v).as_array().ok_or_else(|| anyhow::anyhow!("the PRs: {{\"prs\":[{{\"repo\",\"base\",\"title\",\"changes\",\"files\",\"migration\",\"breaking\",\"escalate\",\"after\"}}]}}"))?;
    if list.len() > MAX_PRS {
        bail!("{} PRs: a plan predicts {MAX_PRS} at most", list.len());
    }
    let names: Vec<&str> = repos.iter().map(|r| folder_name(r)).collect();
    let mut out: Vec<Predicted> = vec![];
    for (i, x) in list.iter().enumerate() {
        let n = i as u32 + 1;
        let s = |k: &str| x[k].as_str().unwrap_or("").trim().to_string();
        let repo = s("repo");
        let Some(name) = names.iter().find(|r| r.eq_ignore_ascii_case(folder_name(&repo))).filter(|_| !repo.is_empty()) else {
            bail!("PR {n}: its repo {repo:?} isn't one of the draft's ({})", names.join(", "));
        };
        let title = s("title");
        if title.is_empty() {
            bail!("PR {n} ({name}): it has no title");
        }
        let base = match s("base") {
            b if b.is_empty() => "dev".to_string(),
            b => b,
        };
        let breaking = match &x["breaking"] {
            Value::Bool(true) => "the plan says it breaks something".to_string(),
            Value::String(t) => t.trim().to_string(),
            _ => String::new(),
        };
        let (checks, edge_cases) = validation_of(x, n, name)?;
        let (dependents, outside) = dependents_of(x, n, name)?;
        let mut p = Predicted {
            n,
            repo: name.to_string(),
            base,
            title,
            changes: s("changes"),
            files: strings(&x["files"]),
            migration: x["migration"].as_bool().unwrap_or(false),
            breaking,
            escalate: strings(&x["escalate"]),
            after: x["after"].as_array().into_iter().flatten().filter_map(Value::as_u64).map(|a| a as u32).filter(|a| *a != n).collect(),
            checks,
            edge_cases,
            dependents,
            outside,
            ..Default::default()
        };
        p.score = score(&p.evidence());
        out.push(p);
    }
    for p in &out {
        if let Some(a) = p.after.iter().find(|a| **a == 0 || **a as usize > out.len()) {
            bail!("PR {} waits for PR {a}, which isn't in the list", p.n);
        }
    }
    Ok(out)
}

/// A predicted PR's checks and edge cases, as the planner wrote them: each
/// check says what it shows, how (the exact test, command or live step)
/// and its evidence; each edge case names the checks that cover it.
fn validation_of(x: &Value, n: u32, name: &str) -> Result<(Vec<Check>, Vec<EdgeCase>)> {
    let pr = format!("PR {n} ({name})");
    let list = x["checks"].as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
        anyhow::anyhow!("{pr} has no checks: say how it is validated for sure, \"checks\": [{{\"id\": \"c1\", \"what\": \"<what it shows>\", \"how\": \"<the exact test, command or live step>\", \"evidence\": \"<test output, a screenshot, a log>\"}}]")
    })?;
    let mut checks: Vec<Check> = vec![];
    for (i, c) in list.iter().enumerate() {
        let s = |k: &str| c[k].as_str().unwrap_or("").trim().to_string();
        let id = match s("id") {
            id if id.is_empty() => format!("c{}", i + 1),
            id => id,
        };
        if checks.iter().any(|x| x.id.eq_ignore_ascii_case(&id)) {
            bail!("{pr}: two checks are {id}");
        }
        for (k, says) in [("what", "what it shows"), ("how", "the exact test, command or live step"), ("evidence", "the evidence it produces (test output, a screenshot, a log)")] {
            if s(k).is_empty() {
                bail!("{pr}, check {id}: no \"{k}\" ({says})");
            }
        }
        checks.push(Check { id, what: s("what"), how: s("how"), evidence: s("evidence"), status: PLANNED.into(), ..Default::default() });
    }
    let list = x["edge_cases"].as_array().filter(|a| !a.is_empty()).ok_or_else(|| {
        anyhow::anyhow!("{pr} has no edge cases: \"edge_cases\": [{{\"case\": \"<the edge case>\", \"checks\": [\"<the checks that cover it>\"]}}]")
    })?;
    let mut edges: Vec<EdgeCase> = vec![];
    for e in list {
        let case = e["case"].as_str().or(e.as_str()).unwrap_or("").trim().to_string();
        if case.is_empty() {
            bail!("{pr}: an edge case with no \"case\"");
        }
        let mut by = strings(&e["checks"]);
        by.extend(strings(&e["check"]));
        if by.is_empty() {
            bail!("{pr}, edge case \"{case}\": name the check that covers it (\"checks\": [\"c1\"])");
        }
        let mut ids = vec![];
        for b in by {
            let Some(c) = checks.iter().find(|c| c.id.eq_ignore_ascii_case(&b)) else {
                bail!("{pr}, edge case \"{case}\": no check {b} (its checks are {})", checks.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", "));
            };
            ids.push(c.id.clone());
        }
        edges.push(EdgeCase { case, checks: ids });
    }
    Ok((checks, edges))
}

/// Who outside depends on what a predicted PR changes, as the planner
/// found it (`dependents`: what, used by whom, whether it breaks), and how
/// it checked (`outside`). One of them is needed: a PR nothing depends on says so.
fn dependents_of(x: &Value, n: u32, name: &str) -> Result<(Vec<Uses>, String)> {
    let pr = format!("PR {n} ({name})");
    let mut out = vec![];
    for d in x["dependents"].as_array().into_iter().flatten() {
        let s = |k: &str| d[k].as_str().unwrap_or("").trim().to_string();
        let (what, used_by) = (s("what"), s("used_by"));
        if what.is_empty() || used_by.is_empty() {
            bail!("{pr}: a dependent needs \"what\" (what the PR changes that it uses) and \"used_by\" (repo, file, function)");
        }
        out.push(Uses { what, used_by, breaks: d["breaks"].as_bool().unwrap_or(false) });
    }
    let outside = x["outside"].as_str().unwrap_or("").trim().to_string();
    if out.is_empty() && outside.is_empty() {
        bail!(
            "{pr}: say who outside depends on what it changes: \"dependents\": [{{\"what\": \"<what it changes>\", \"used_by\": \"<repo, file, function>\", \"breaks\": true}}], or \"outside\": \"<nothing outside depends on it: what you searched>\""
        );
    }
    Ok((out, outside))
}

/// The predicted PRs in a few lines, as `tower plan prs` prints them for
/// the planner (to put in the plan as they are) and `tower plan show` says.
pub fn said(prs: &[Predicted]) -> String {
    prs.iter()
        .map(|p| {
            let mut s = format!("PR {} · {} → {}: {}\n  {}", p.n, p.repo, p.base, p.title, p.score.line);
            for c in &p.checks {
                s += &format!("\n  check {}: {} (how: {}; evidence: {})", c.id, c.what, c.how, c.evidence);
            }
            for e in &p.edge_cases {
                s += &format!("\n  edge case: {} (covered by {})", e.case, e.checks.join(", "));
            }
            for d in &p.dependents {
                s += &format!("\n  {}: {}", if d.breaks { "BREAKS" } else { "depends on it, still works" }, d.said());
            }
            if p.dependents.is_empty() {
                s += &format!("\n  nothing outside depends on it: {}", p.outside);
            } else if !p.outside.is_empty() {
                s += &format!("\n  checked: {}", p.outside);
            }
            if !p.after.is_empty() {
                s += &format!("\n  after PR {}", p.after.iter().map(u32::to_string).collect::<Vec<_>>().join(", PR "));
            }
            if let Some(k) = &p.pr {
                s += &format!("\n  opened: {k}");
            }
            s
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---- the real PR ----

/// The words of a title worth comparing.
fn title_words(t: &str) -> Vec<String> {
    tokens(t).into_iter().filter(|w| w.len() >= 3 && !matches!(w.as_str(), "the" | "and" | "for" | "with" | "into" | "from" | "feat" | "fix" | "chore")).collect()
}

/// The prediction a real PR (`repo` as `owner/name`, into `base`, titled
/// `title`) is: one of the same repo not matched yet, the same base first,
/// then the one whose title shares the most words, then the first in order.
pub fn best_match(prs: &[Predicted], repo: &str, base: &str, title: &str) -> Option<usize> {
    let name = folder_name(repo);
    let words = title_words(title);
    prs.iter()
        .enumerate()
        .filter(|(_, p)| p.pr.is_none() && p.repo.eq_ignore_ascii_case(name))
        .map(|(i, p)| {
            let shared = title_words(&p.title).iter().filter(|w| words.contains(w)).count();
            (i, (p.base.eq_ignore_ascii_case(base), shared))
        })
        .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(&a.0)))
        .map(|(i, _)| i)
}

// ---- merging it without asking ----

/// The branches a low-risk PR may be merged into by the tower.
pub fn is_dev(base: &str) -> bool {
    matches!(base.trim().to_lowercase().as_str(), "dev" | "develop" | "development")
}

/// The tower's own merge of a PR, as it went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Auto {
    /// `merged` or `failed`.
    pub state: String,
    /// Why it failed; how it went.
    pub why: String,
    /// The head commit it was for.
    pub sha: String,
    pub at_ms: u64,
}

/// Where a PR stands with the tower's auto-merge.
#[derive(Debug, Clone, PartialEq)]
pub enum Path {
    /// The setting is off: every merge is the user's.
    Off,
    /// The user's to merge: why.
    Yours(String),
    /// The tower will merge it once this is done: what it waits for.
    Waits(String),
    /// The tower merges it now.
    Ready,
}

/// Whether the tower merges `pr` itself (`on`: `[prs] auto_merge_low_risk`):
/// only a PR into dev that a plan predicted, scored low risk, every planned
/// check passed (one not reported yet is waited for; one failed or not run
/// makes it the user's), open and no draft, clean, whose CI checks pass and whose reviewer agreed without
/// finding anything critical, and that the tower hasn't failed to merge at
/// this commit.
pub fn auto_path(pr: &Pr, on: bool) -> Path {
    let yours = |s: &str| Path::Yours(s.to_string());
    if !on {
        return Path::Off;
    }
    if !pr.open() {
        return yours("it isn't open");
    }
    if let Some(a) = pr.auto.as_ref().filter(|a| a.state == "failed" && a.sha == pr.head_sha) {
        return Path::Yours(format!("the tower couldn't merge it: {}", a.why));
    }
    let Some(s) = &pr.score else { return yours("it has no review score") };
    if s.needs_review {
        return Path::Yours(s.line.clone());
    }
    // Who depends on what it changes: checked at its head, and nothing breaks.
    match &pr.impact {
        None => return yours("who depends on what it changes wasn't checked"),
        Some(i) if i.breaks() => return Path::Yours(i.line()),
        Some(i) if i.sha != pr.head_sha => return Path::Waits("its dependents checked at its head commit".into()),
        Some(_) => {}
    }
    if s.planned.is_none() {
        return yours("no plan predicted it");
    }
    if !is_dev(&pr.base) {
        return Path::Yours(if crate::prs::is_prod(&pr.base) {
            format!("it merges into {}: that ships to production", pr.base)
        } else {
            format!("it merges into {}, not dev", if pr.base.is_empty() { "?" } else { &pr.base })
        });
    }
    // Its planned checks, all passed with their evidence: else never low risk enough.
    match &pr.validation {
        None => return yours("its plan lists no checks for it"),
        Some(v) if v.checks.is_empty() => return yours("its plan lists no checks for it"),
        Some(v) if v.any_bad() => return Path::Yours(format!("planned checks missing: {}", v.missing_line())),
        Some(v) if !v.all_passed() => {
            return Path::Waits(format!("its planned checks ({})", v.missing().iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ")));
        }
        Some(_) => {}
    }
    if pr.draft {
        return yours("it's a draft");
    }
    if pr.mergeable == "conflicting" {
        return yours("it has conflicts");
    }
    match pr.checks.state.as_str() {
        "failing" => return yours("its checks fail"),
        "none" => return yours("no checks ran on it"),
        "pending" => return Path::Waits("its checks".into()),
        _ => {}
    }
    match &pr.review {
        None => return yours("no reviewer looked at it"),
        Some(r) if r.status == "agreed" && r.rounds == 0 && r.critical == 0 => {}
        Some(r) if r.status == "reviewing" && r.rounds == 0 => return Path::Waits("its reviewer".into()),
        Some(r) if r.status == "agreed" => return Path::Yours(format!("its reviewer found critical issues first ({} round{})", r.rounds, if r.rounds == 1 { "" } else { "s" })),
        Some(r) if r.status == "ended" => return yours("its review ended without agreement"),
        Some(_) => return yours("its reviewer found critical issues"),
    }
    if pr.mergeable != "mergeable" {
        return Path::Waits("GitHub to say it merges cleanly".into());
    }
    if let Some(b) = crate::prs::gate(pr).blocked {
        return Path::Yours(b);
    }
    // The score was read at this commit (a push since may have brought anything).
    if s.sha != pr.head_sha {
        return Path::Waits("its files read at its head commit".into());
    }
    Path::Ready
}

/// What the user decides about: the PRs as they are, but those the tower
/// is on its way to merging itself (`Waits`, `Ready`) wait for nobody.
pub fn for_the_user(prs: &[Pr], on: bool) -> Vec<Pr> {
    prs.iter()
        .map(|p| match auto_path(p, on) {
            Path::Waits(_) | Path::Ready if p.waiting => Pr { waiting: false, ..p.clone() },
            _ => p.clone(),
        })
        .collect()
}

/// What a PR's card says of the tower's auto-merge, when there is
/// anything to say: it was merged by the tower, it will be, or why not.
pub fn auto_line(pr: &Pr, on: bool) -> Option<String> {
    if let Some(a) = pr.auto.as_ref().filter(|a| a.state == "merged") {
        return Some(format!("The tower merged it: {}", a.why));
    }
    match auto_path(pr, on) {
        Path::Off => pr.score.as_ref().filter(|s| !s.needs_review && s.planned.is_some() && is_dev(&pr.base)).map(|_| "Auto-merge is off: yours to merge.".to_string()),
        Path::Waits(w) => Some(format!("The tower merges it itself after {w}.")),
        Path::Ready => Some("The tower is merging it.".into()),
        Path::Yours(_) if pr.score.as_ref().is_some_and(|s| s.needs_review) => None,
        Path::Yours(why) => Some(format!("Yours to merge: {why}.")),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    fn ev(title: &str, changes: &str, files: &[&str]) -> Evidence {
        Evidence { title: title.into(), changes: changes.into(), files: files.iter().map(|f| f.to_string()).collect(), ..Default::default() }
    }

    #[test]
    fn a_migration_needs_review_and_a_plain_ui_change_is_low_risk() {
        let m = score(&ev("Add the orders table", "A table for orders, read by the API.", &["db/migrations/0042_orders.sql", "src/orders/model.ts"]));
        assert!(m.needs_review);
        assert_eq!(m.flags, ["migration"]);
        assert_eq!(m.line, "Needs review: a database migration (db/migrations/0042_orders.sql)");
        let ui = score(&ev("Show the order total on the cart page", "The cart shows the total, styled like the rest.", &["src/components/Cart.tsx", "src/styles/cart.css"]));
        assert!(!ui.needs_review, "{ui:?}");
        assert_eq!(ui.line, LOW_LINE);
        assert_eq!((ui.word(), ui.reason()), ("Low risk", "no migration, breaking change, secrets, infrastructure or auth"));
        assert_eq!((m.word(), m.reason()), ("Needs review", "a database migration (db/migrations/0042_orders.sql)"));
    }

    #[test]
    fn what_the_planner_declares_counts_first_and_said_as_not_done_doesnt() {
        let declared = Evidence { title: "Orders API v2".into(), migration: true, breaking: "renames the total field to amount".into(), ..Default::default() };
        let s = score(&declared);
        assert_eq!(s.flags, ["migration", "breaking"]);
        assert_eq!(s.line, "Needs review: a database migration (the plan says so); a breaking change (renames the total field to amount)");
        // "No migration", "without breaking changes": not a reason.
        let calm = score(&ev("Tidy the cart", "No migration, no breaking change, without new dependencies.", &["src/Cart.tsx"]));
        assert!(!calm.needs_review, "{calm:?}");
        // Renaming a button isn't renaming something public; renaming a field is.
        assert!(!score(&ev("Rename the Save button to Submit", "", &[])).needs_review);
        let renamed = score(&ev("Orders", "Renames the total field in the response to amount.", &[]));
        assert_eq!(renamed.flags, ["breaking"], "{renamed:?}");
        // A release build isn't a release; a release is.
        assert!(!score(&ev("Faster release build", "", &[])).needs_review);
        assert_eq!(score(&ev("release: dev into main", "", &[])).flags, ["deploy"]);
        // The escalate list in the planner's words, filed by its kind.
        let e = Evidence { title: "Webhooks".into(), escalate: vec!["a new AWS SQS queue".into(), "something only the user knows".into()], ..Default::default() };
        assert_eq!(score(&e).flags, ["infrastructure", "escalate"]);
    }

    #[test]
    fn files_say_what_nobody_said() {
        let kind = |p: &str| file_kind(p);
        for (p, k) in [
            ("db/migrations/20261113_add_orders.sql", Some("migration")),
            ("prisma/schema.prisma", Some("migration")),
            ("api/openapi.yaml", Some("breaking")),
            ("proto/orders.proto", Some("breaking")),
            (".env.production", Some("secrets")),
            (".env.example", None),
            ("infra/main.tf", Some("infrastructure")),
            (".github/workflows/deploy.yml", Some("infrastructure")),
            ("serverless.yml", Some("infrastructure")),
            ("Dockerfile", Some("infrastructure")),
            ("src/auth/session.ts", Some("auth")),
            ("src/components/AuthProvider.tsx", Some("auth")),
            ("src/pages/sign-in.tsx", Some("auth")),
            ("src/components/AuthorCard.tsx", None),
            ("src/billing/invoice.ts", Some("money")),
            ("package.json", Some("dependencies")),
            ("package-lock.json", None),
            ("src/components/Cart.tsx", None),
            ("README.md", None),
        ] {
            assert_eq!(kind(p), k, "{p}");
        }
        let s = score(&ev("UI", "", &["src/a.tsx", "db/migrations/1.sql", "db/migrations/2.sql", ".github/workflows/ci.yml"]));
        assert_eq!(s.reasons, ["a database migration (db/migrations/1.sql and 1 more)", "infrastructure (.github/workflows/ci.yml)"]);
    }

    fn draft_repos() -> Vec<String> {
        vec!["/w/eyz/eyz-customer-support".into(), "/w/eyz/eyzflow-admin-dashboard".into()]
    }

    /// The plan of the validation: a migration PR and a plain UI PR.
    pub(crate) fn two_prs() -> Value {
        json!({ "prs": [
            { "repo": "eyz-customer-support", "title": "Store each message's translation", "changes": "A translations table and the endpoint that fills it.",
              "files": ["db/migrations/0042_message_translations.sql", "src/messages/translate.ts"], "migration": true,
              "checks": [
                { "id": "c1", "what": "The migration creates message_translations and rolls back cleanly", "how": "npm run db:migrate, then npm run db:rollback, on a scratch database", "evidence": "the migrate and rollback output" },
                { "id": "c2", "what": "The endpoint stores one translation per message and language", "how": "add src/messages/translate.test.ts and run npm test -- translate", "evidence": "test output" },
                { "id": "c3", "what": "On dev, translating a real message stores its row", "how": "after the deploy to dev, call the endpoint for one message and query the table", "evidence": "the response and the row, in a log" },
              ],
              "edge_cases": [
                { "case": "A message already translated: no second row", "checks": ["c2"] },
                { "case": "An unsupported language is refused", "checks": ["c2"] },
                { "case": "Rolling back after rows exist", "checks": ["c1"] },
              ],
              "dependents": [{ "what": "GET /v1/messages and its response", "used_by": "eyzflow-admin-dashboard src/inbox/api.ts loadMessages", "breaks": false }],
              "outside": "rg over ~/Work for /v1/messages and message_translations: the field is added, nothing is renamed or removed" },
            { "repo": "eyzflow-admin-dashboard", "title": "Show translated messages in the inbox", "changes": "The inbox shows each message's translation under it, with a toggle.",
              "files": ["src/inbox/MessageRow.tsx", "src/inbox/TranslationToggle.tsx"], "after": [1],
              "checks": [
                { "id": "c1", "what": "Each message shows its translation, and the toggle shows the original", "how": "add src/inbox/MessageRow.test.tsx and run npm test -- MessageRow", "evidence": "test output" },
                { "id": "c2", "what": "On dev, the inbox shows translated messages to a German-speaking agent", "how": "open the inbox on dev as that agent and toggle one message", "evidence": "two screenshots: translated and original" },
              ],
              "edge_cases": [
                { "case": "A message with no translation yet shows the original, no empty line", "checks": ["c1"] },
                { "case": "A very long message wraps, nothing is cut", "checks": ["c2"] },
              ],
              "outside": "rg over ~/Work for MessageRow and TranslationToggle: only the inbox uses them" },
        ]})
    }

    #[test]
    fn the_planners_list_is_checked_against_the_draft_and_scored() {
        let prs = check(&two_prs(), &draft_repos()).unwrap();
        assert_eq!(prs.len(), 2);
        assert_eq!((prs[0].n, prs[0].repo.as_str(), prs[0].base.as_str()), (1, "eyz-customer-support", "dev"), "dev when not said");
        assert!(prs[0].score.needs_review && prs[0].score.flags == ["migration"], "{:?}", prs[0].score);
        assert_eq!(prs[0].score.line, "Needs review: a database migration (db/migrations/0042_message_translations.sql)");
        assert!(!prs[1].score.needs_review, "{:?}", prs[1].score);
        assert_eq!((prs[1].after.as_slice(), prs[1].files.len()), ([1].as_slice(), 2));
        // The list alone reads too; a whole path is its folder's name.
        let ok_checks = json!([{ "what": "w", "how": "h", "evidence": "e" }]);
        let ok_edges = json!([{ "case": "c", "check": "c1" }]);
        let bare = json!([{ "repo": "/w/eyz/eyzflow-admin-dashboard/", "title": "x", "base": "main", "files": "a.tsx, b.tsx", "checks": ok_checks, "edge_cases": ok_edges, "outside": "nothing" }]);
        let p = check(&bare, &draft_repos()).unwrap();
        assert_eq!((p[0].repo.as_str(), p[0].base.as_str(), p[0].files.len()), ("eyzflow-admin-dashboard", "main", 2));
        // What can't be.
        assert!(check(&json!({"prs":[{"repo":"nope","title":"x"}]}), &draft_repos()).unwrap_err().to_string().contains("isn't one of the draft's (eyz-customer-support, eyzflow-admin-dashboard)"));
        assert!(check(&json!({"prs":[{"repo":"eyz-customer-support"}]}), &draft_repos()).unwrap_err().to_string().contains("no title"));
        assert!(check(&json!({"prs":[{"repo":"eyz-customer-support","title":"x","after":[4]}]}), &draft_repos()).is_err());
        assert!(check(&json!({"nope":1}), &draft_repos()).is_err());
        // Each PR's validation: its checks (ids given or numbered), its edge cases tied to them, none reported yet.
        assert_eq!(prs[0].checks.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), ["c1", "c2", "c3"]);
        assert_eq!((prs[0].checks[0].how.as_str(), prs[0].checks[0].status.as_str()), ("npm run db:migrate, then npm run db:rollback, on a scratch database", PLANNED));
        assert_eq!(prs[0].edge_cases[2], EdgeCase { case: "Rolling back after rows exist".into(), checks: vec!["c1".into()] });
        assert_eq!((prs[1].checks.len(), prs[1].edge_cases.len()), (2, 2));
        assert_eq!(p[0].checks[0].id, "c1", "numbered when not given");
        let v = prs[1].validation();
        assert_eq!((v.all_passed(), v.missing().len(), v.line().as_str()), (false, 2, "Checks missing: c1 Each message shows its translation, and the toggle shows the… (not reported yet), c2 On dev, the inbox shows translated messages to a German-spea… (not reported yet) (0 of 2 passed)"));
        // What the planner must give: checks with what, how and evidence; edge cases each covered by a check.
        let one = |checks: Value, edges: Value| check(&json!([{ "repo": "eyz-customer-support", "title": "x", "checks": checks, "edge_cases": edges, "outside": "nothing" }]), &draft_repos());
        assert!(one(json!([]), ok_edges.clone()).unwrap_err().to_string().contains("has no checks: say how it is validated for sure"));
        assert!(one(json!([{ "what": "w", "evidence": "e" }]), ok_edges.clone()).unwrap_err().to_string().contains("check c1: no \"how\""));
        assert!(one(json!([{ "what": "w", "how": "h" }]), ok_edges.clone()).unwrap_err().to_string().contains("no \"evidence\""));
        assert!(one(ok_checks.clone(), json!([])).unwrap_err().to_string().contains("has no edge cases"));
        assert!(one(ok_checks.clone(), json!([{ "case": "c" }])).unwrap_err().to_string().contains("name the check that covers it"));
        assert!(one(ok_checks.clone(), json!([{ "case": "c", "checks": ["c7"] }])).unwrap_err().to_string().contains("no check c7 (its checks are c1)"));
        assert!(one(json!([{ "id": "a", "what": "w", "how": "h", "evidence": "e" }, { "id": "A", "what": "w", "how": "h", "evidence": "e" }]), ok_edges.clone()).unwrap_err().to_string().contains("two checks are"));
        let said = said(&prs);
        assert!(said.contains("\n  check c1: The migration creates message_translations and rolls back cleanly (how: npm run db:migrate"), "{said}");
        assert!(said.contains("\n  edge case: Rolling back after rows exist (covered by c1)"), "{said}");
        assert!(said.contains("PR 1 · eyz-customer-support → dev: Store each message's translation\n  Needs review: a database migration"), "{said}");
        assert!(said.contains("PR 2 · eyzflow-admin-dashboard → dev: Show translated messages in the inbox\n  Low risk:") && said.contains("after PR 1"), "{said}");
    }

    #[test]
    fn a_planned_pr_says_who_depends_on_it_and_one_that_breaks_them_is_a_breaking_change() {
        let base = || json!({ "repo": "eyz-customer-support", "title": "Messages carry their translation", "changes": "The messages response sends the translation under a new key.",
            "checks": [{ "what": "the dashboard still reads it", "how": "npx tsc --noEmit in eyzflow-admin-dashboard, then npm test -- inbox", "evidence": "the output" }],
            "edge_cases": [{ "case": "an old client", "checks": ["c1"] }] });
        // Nothing said about who depends on it: refused.
        assert!(check(&json!([base()]), &draft_repos()).unwrap_err().to_string().contains("say who outside depends on what it changes"));
        // A dependent it breaks: a breaking change, naming what breaks and who uses it.
        let mut b = base();
        b["dependents"] = json!([
            { "what": "the response field translated_text", "used_by": "eyzflow-admin-dashboard src/inbox/api.ts loadMessages", "breaks": true },
            { "what": "GET /v1/messages", "used_by": "eyz-mobile src/sync.kt syncAll", "breaks": false },
        ]);
        b["outside"] = json!("rg over ~/Work for translated_text and /v1/messages");
        let p = check(&json!([b]), &draft_repos()).unwrap();
        assert!(p[0].score.needs_review && p[0].score.flags == ["breaking"], "{:?}", p[0].score);
        assert_eq!(p[0].score.line, "Needs review: a breaking change (the response field translated_text, used by eyzflow-admin-dashboard src/inbox/api.ts loadMessages)");
        let text = said(&p);
        assert!(text.contains("BREAKS: the response field translated_text, used by eyzflow-admin-dashboard src/inbox/api.ts loadMessages"), "{text}");
        assert!(text.contains("depends on it, still works: GET /v1/messages, used by eyz-mobile src/sync.kt syncAll") && text.contains("checked: rg over ~/Work"), "{text}");
        // Dependents it doesn't break, or none at all (said so): not a breaking change.
        let mut fine = base();
        fine["outside"] = json!("rg over ~/Work for translated_text: nothing outside reads it");
        let p = check(&json!([fine]), &draft_repos()).unwrap();
        assert!(!p[0].score.flags.contains(&"breaking".to_string()), "{:?}", p[0].score);
        assert!(said(&p).contains("nothing outside depends on it: rg over ~/Work for translated_text"));
        // A dependent needs what and who.
        let mut bad = base();
        bad["dependents"] = json!([{ "what": "x" }]);
        assert!(check(&json!([bad]), &draft_repos()).unwrap_err().to_string().contains("\"used_by\""));
    }

    #[test]
    fn a_real_pr_finds_its_prediction_and_is_scored_on_its_own_files_too() {
        let mut prs = check(&two_prs(), &draft_repos()).unwrap();
        prs.push(Predicted { n: 3, repo: "eyzflow-admin-dashboard".into(), base: "dev".into(), title: "Settings page for translation language".into(), ..Default::default() });
        // The same repo, the closest title.
        assert_eq!(best_match(&prs, "eyzhub/eyzflow-admin-dashboard", "dev", "feat: translated messages in the inbox"), Some(1));
        assert_eq!(best_match(&prs, "eyzhub/eyzflow-admin-dashboard", "dev", "language settings page"), Some(2));
        assert_eq!(best_match(&prs, "eyzhub/other", "dev", "anything"), None);
        prs[1].pr = Some("eyzhub/eyzflow-admin-dashboard#7".into());
        assert_eq!(best_match(&prs, "eyzhub/eyzflow-admin-dashboard", "dev", "translated messages"), Some(2), "a prediction is matched once");
        // Its own files: low risk as predicted; or a migration the plan didn't predict.
        let ui = &prs[1].score;
        let same = combine(Some(ui), &real_score("Show translated messages", &["src/inbox/MessageRow.tsx".into()]), "abc");
        assert!(!same.needs_review && same.sha == "abc");
        let worse = combine(Some(ui), &real_score("Show translated messages", &["src/inbox/MessageRow.tsx".into(), "db/migrations/0043_flags.sql".into()]), "def");
        assert!(worse.needs_review);
        assert_eq!(worse.line, "Needs review: a database migration (db/migrations/0043_flags.sql), which the plan didn't predict");
        // No plan: its own files alone.
        let alone = combine(None, &real_score("Infra", &["infra/main.tf".into()]), "x");
        assert_eq!((alone.needs_review, alone.line.as_str()), (true, "Needs review: infrastructure (infra/main.tf)"));
        assert_eq!(files_of(&json!({"files":[{"path":"a.ts","additions":3},{"path":"b.sql"}]})), ["a.ts", "b.sql"]);
    }

    fn low_pr(base: &str) -> Pr {
        Pr {
            repo: "o/app".into(),
            number: 7,
            base: base.into(),
            head_sha: "abc".into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            waiting: true,
            checks: crate::prs::Checks { state: "passing".into(), passed: 1, total: 1, ..Default::default() },
            review: Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }),
            score: Some(Score { line: LOW_LINE.into(), planned: Some("p1#2".into()), sha: "abc".into(), ..Default::default() }),
            validation: Some(Validation {
                checks: vec![Check { id: "c1".into(), what: "It works".into(), how: "npm test".into(), evidence: "test output".into(), status: PASSED.into(), ..Default::default() }],
                edge_cases: vec![EdgeCase { case: "empty".into(), checks: vec!["c1".into()] }],
            }),
            impact: Some(crate::impact::Impact { sha: "abc".into(), searched: vec!["shop-web".into()], ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn only_a_planned_low_risk_pr_into_dev_that_passed_everything_merges_by_itself() {
        let pr = low_pr("dev");
        assert_eq!(auto_path(&pr, true), Path::Ready);
        assert_eq!(auto_path(&pr, false), Path::Off, "the setting is off: the user's");
        let with = |f: &dyn Fn(&mut Pr)| {
            let mut p = low_pr("dev");
            f(&mut p);
            auto_path(&p, true)
        };
        assert_eq!(auto_path(&low_pr("main"), true), Path::Yours("it merges into main: that ships to production".into()));
        assert_eq!(auto_path(&low_pr("feature/x"), true), Path::Yours("it merges into feature/x, not dev".into()));
        assert!(matches!(with(&|p| p.score = Some(score(&ev("x", "", &["db/migrations/1.sql"])))), Path::Yours(w) if w.starts_with("Needs review: a database migration")));
        assert_eq!(with(&|p| p.score.as_mut().unwrap().planned = None), Path::Yours("no plan predicted it".into()));
        assert_eq!(with(&|p| p.score = None), Path::Yours("it has no review score".into()));
        // Who depends on what it changes: checked at its head, and nothing breaks.
        assert_eq!(with(&|p| p.impact = None), Path::Yours("who depends on what it changes wasn't checked".into()));
        assert_eq!(with(&|p| p.impact.as_mut().unwrap().sha = "old".into()), Path::Waits("its dependents checked at its head commit".into()));
        let broken = |p: &mut Pr| {
            let i = p.impact.as_mut().unwrap();
            i.things = vec![crate::impact::Thing { kind: "function".into(), name: "formatTotal".into(), change: "removed".into(), file: "src/money.ts".into(), ..Default::default() }];
            i.dependents = vec![crate::impact::Dependent { repo: "shop-web".into(), file: "src/cart/Total.tsx".into(), line: 4, uses: "formatTotal".into(), within: "renderTotal".into(), ..Default::default() }];
        };
        assert_eq!(with(&broken), Path::Yours("Breaking change: removes formatTotal, used by shop-web src/cart/Total.tsx:4 in renderTotal".into()));
        assert_eq!(with(&|p| p.checks.state = "pending".into()), Path::Waits("its checks".into()));
        assert_eq!(with(&|p| p.checks.state = "failing".into()), Path::Yours("its checks fail".into()));
        assert_eq!(with(&|p| p.checks.state = "none".into()), Path::Yours("no checks ran on it".into()));
        assert_eq!(with(&|p| p.review = None), Path::Yours("no reviewer looked at it".into()));
        assert_eq!(with(&|p| p.review.as_mut().unwrap().status = "reviewing".into()), Path::Waits("its reviewer".into()));
        // The reviewer found something, even once: the user's.
        assert!(matches!(with(&|p| { let r = p.review.as_mut().unwrap(); r.rounds = 1; }), Path::Yours(w) if w.contains("critical issues first")));
        assert!(matches!(with(&|p| { let r = p.review.as_mut().unwrap(); (r.status, r.rounds, r.critical) = ("changes".into(), 1, 2); }), Path::Yours(_)));
        assert_eq!(with(&|p| p.draft = true), Path::Yours("it's a draft".into()));
        assert_eq!(with(&|p| p.mergeable = "conflicting".into()), Path::Yours("it has conflicts".into()));
        assert_eq!(with(&|p| p.mergeable = "unknown".into()), Path::Waits("GitHub to say it merges cleanly".into()));
        // A push since its files were read: they are read again first.
        assert_eq!(with(&|p| p.head_sha = "new".into()), Path::Waits("its dependents checked at its head commit".into()));
        assert_eq!(
            with(&|p| {
                p.head_sha = "new".into();
                p.impact.as_mut().unwrap().sha = "new".into();
            }),
            Path::Waits("its files read at its head commit".into())
        );
        // The tower couldn't merge it at this commit: the user's; a new commit tries again.
        let failed = Auto { state: "failed".into(), why: "a review is required".into(), sha: "abc".into(), at_ms: 1 };
        assert_eq!(with(&|p| p.auto = Some(failed.clone())), Path::Yours("the tower couldn't merge it: a review is required".into()));
        // On the tower's way: no merge decision for the user meanwhile; the rest as they are.
        let list = [low_pr("dev"), low_pr("main"), Pr { checks: crate::prs::Checks { state: "pending".into(), ..Default::default() }, ..low_pr("dev") }];
        let waits: Vec<bool> = for_the_user(&list, true).iter().map(|p| p.waiting).collect();
        assert_eq!(waits, [false, true, false]);
        assert_eq!(for_the_user(&list, false).iter().filter(|p| p.waiting).count(), 3, "off: all the user's");
        // What its card says.
        assert_eq!(auto_line(&low_pr("dev"), false).as_deref(), Some("Auto-merge is off: yours to merge."));
        assert_eq!(auto_line(&list[2], true).as_deref(), Some("The tower merges it itself after its checks."));
        assert_eq!(auto_line(&low_pr("main"), true).as_deref(), Some("Yours to merge: it merges into main: that ships to production."));
        let migration = Pr { score: Some(score(&ev("x", "", &["db/migrations/1.sql"]))), ..low_pr("dev") };
        assert_eq!(auto_line(&migration, true), None, "its score says it");
    }
}
