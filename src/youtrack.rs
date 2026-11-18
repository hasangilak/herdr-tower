//! A YouTrack ticket for each PR a session opens in a repo of the GitHub
//! owners `[youtrack] github_owners` names (`eyzhub`), when `[youtrack]
//! enabled` is on (off by default: `tower youtrack on|off`).
//!
//! - **What it holds**: the PR's title (its summary), and in its
//!   description a link to the PR, its repo and branches, the space and
//!   session that made it, and what it changes in a sentence or two (the
//!   merge card's summary, `crate::describe`; else its description's
//!   opening; else that it has none yet).
//! - **Linked both ways**: the ticket links the PR; the PR gets one comment
//!   with the ticket's link (`[youtrack] link_pr`), marked (`MARK`) so it is
//!   found again.
//! - **Never twice**: the ticket is kept with the PR (`Pr::ticket`, in
//!   `prs.json`); one the tower made but didn't keep (it stopped between the
//!   two) is found first, on the PR's comments, then on YouTrack by the
//!   PR's link in a ticket's description, and kept, not made again. When
//!   YouTrack can't be searched, no ticket is made until it can.
//! - **When**: an open PR (a draft too) whose description the tower has
//!   looked at, or five minutes after it was found; a closed or merged one
//!   never gets one.
//! - **YouTrack's REST API** (`/api/issues`), through curl. The token is in
//!   the macOS Keychain (`tower youtrack token`), read when it is needed,
//!   handed to curl on its stdin (never in its arguments, never in a log,
//!   never in config.toml or git). `HERDER_TOWER_YOUTRACK_TOKEN` stands in
//!   for it in tests.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::YoutrackCfg;
use crate::prs::Pr;

/// Where the token is kept in the Keychain.
pub const KEYCHAIN_SERVICE: &str = "herder-tower-youtrack";
pub const KEYCHAIN_ACCOUNT: &str = "token";
/// The mark on the PR comment that links its ticket (an HTML comment: not shown).
pub const MARK: &str = "herder-tower-youtrack";
/// A PR whose description the tower hasn't seen yet waits this long for it.
pub const WAIT_FOR_BODY_MS: u64 = 5 * 60_000;

/// A PR's ticket.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ticket {
    /// `EYZ-12`.
    pub id: String,
    pub url: String,
    /// When it was made or found (unix ms).
    pub made_ms: u64,
    /// `made`, `found on the PR` or `found on YouTrack`: a found one wasn't made again.
    pub how: String,
    /// The PR comment that links it (GitHub's id; 0: none yet).
    #[serde(skip_serializing_if = "is_zero")]
    pub comment: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The instance's base URL, without a trailing `/` (nor `/api`).
pub fn base(cfg: &YoutrackCfg) -> String {
    let u = cfg.url.trim().trim_end_matches('/');
    u.strip_suffix("/api").unwrap_or(u).to_string()
}

/// What is missing before tickets can be made (None: nothing).
pub fn missing(cfg: &YoutrackCfg, token: bool) -> Option<String> {
    let mut m = vec![];
    if !base(cfg).starts_with("https://") && !base(cfg).starts_with("http://") {
        m.push("the YouTrack URL (`tower youtrack set url https://…`)");
    }
    if cfg.project.trim().is_empty() {
        m.push("the project (`tower youtrack set project <short name>`)");
    }
    if !token {
        m.push("the API token (`tower youtrack token`)");
    }
    (!m.is_empty()).then(|| m.join(", "))
}

/// A PR in a repo of the owners tickets are for.
pub fn owner_wanted(cfg: &YoutrackCfg, repo: &str) -> bool {
    let owner = repo.split('/').next().unwrap_or("");
    !owner.is_empty() && cfg.github_owners.iter().any(|o| o.trim().eq_ignore_ascii_case(owner))
}

/// What a PR needs now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// A ticket (found or made).
    Ticket,
    /// Its ticket is made; the PR's comment linking it isn't.
    Link,
}

/// What a PR needs now, if anything.
pub fn need(pr: &Pr, cfg: &YoutrackCfg, now: u64) -> Option<Need> {
    if !owner_wanted(cfg, &pr.repo) || pr.number == 0 {
        return None;
    }
    match &pr.ticket {
        Some(t) if !t.id.is_empty() => (cfg.link_pr && t.comment == 0 && pr.open()).then_some(Need::Link),
        _ => (pr.open() && (pr.body.is_some() || now.saturating_sub(pr.found_ms) >= WAIT_FOR_BODY_MS)).then_some(Need::Ticket),
    }
}

/// The PRs that need something now, by key.
pub fn due(prs: &[Pr], cfg: &YoutrackCfg, now: u64) -> Vec<(String, Need)> {
    prs.iter().filter_map(|p| need(p, cfg, now).map(|n| (p.key(), n))).collect()
}

/// What it changes, in a sentence or two: the merge card's summary of its
/// description as it is now, else the description's opening (a few
/// hundred characters at most), else nothing.
pub fn what_it_changes(pr: &Pr) -> String {
    let body = pr.body.as_deref().unwrap_or("").trim();
    if let Some(d) = &pr.described
        && !body.is_empty()
        && d.of == crate::describe::hash(body)
        && !d.summary.trim().is_empty()
    {
        return d.summary.trim().to_string();
    }
    let mut out = String::new();
    for l in body.lines().map(crate::describe::plain) {
        if l.is_empty() || l.starts_with("<!--") || l.starts_with('|') {
            if !out.is_empty() && l.is_empty() {
                break;
            }
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&l);
        if out.chars().count() > 300 {
            break;
        }
    }
    if out.chars().count() > 300 {
        out = format!("{}…", out.chars().take(300).collect::<String>().trim_end());
    }
    out
}

/// The ticket's description (YouTrack markdown).
pub fn description(pr: &Pr, changes: &str) -> String {
    let branches = match (pr.head.as_str(), pr.base.as_str()) {
        ("", "") => String::new(),
        (h, b) => format!(" · `{h}` → `{b}`"),
    };
    let space = pr.space.as_deref().filter(|s| !s.trim().is_empty()).map_or("none".to_string(), |s| format!("**{s}**"));
    let session = match (pr.name.trim(), pr.session.trim()) {
        ("", "") => "unknown".to_string(),
        ("", s) | (s, "") => format!("**{s}**"),
        (n, s) => format!("**{n}** ({s})"),
    };
    let changes = if changes.trim().is_empty() { "The PR has no description yet.".to_string() } else { changes.trim().to_string() };
    format!(
        "**Pull request:** [{repo}#{n}]({url}){draft}\n\
         **Repo:** {repo}{branches}\n\
         **Made in:** space {space}, session {session}\n\n\
         **What it changes:** {changes}\n\n\
         _Opened by Herder Tower for this PR (one ticket per PR): {url}_",
        repo = pr.repo,
        n = pr.number,
        url = pr.url,
        draft = if pr.draft { " (draft)" } else { "" },
    )
}

/// What `POST /api/issues` is sent.
pub fn issue_body(cfg: &YoutrackCfg, project_id: &str, pr: &Pr, changes: &str) -> Value {
    let title = if pr.title.trim().is_empty() { format!("{}#{}", pr.repo, pr.number) } else { pr.title.trim().to_string() };
    let mut v = json!({ "project": { "id": project_id }, "summary": title, "description": description(pr, changes) });
    if !cfg.issue_type.trim().is_empty() {
        v["customFields"] = json!([{ "name": "Type", "$type": "SingleEnumIssueCustomField", "value": { "name": cfg.issue_type.trim() } }]);
    }
    v
}

/// The PR comment that links its ticket.
pub fn comment_text(t: &Ticket) -> String {
    format!("YouTrack ticket: [{id}]({url})\n\n<!-- {MARK} {id} {url} -->", id = t.id, url = t.url)
}

/// The ticket a PR's comments link (the tower's own comment, by its mark),
/// and the comment's id.
pub fn linked_in(comments: &[(u64, String)]) -> Option<(Ticket, u64)> {
    comments.iter().find_map(|(cid, body)| {
        let rest = body.split(&format!("<!-- {MARK} ")).nth(1)?;
        let mut words = rest.split_whitespace();
        let (id, url) = (words.next()?, words.next()?);
        Some((Ticket { id: id.to_string(), url: url.to_string(), how: "found on the PR".into(), comment: *cid, ..Default::default() }, *cid))
    })
}

/// The PR's link in `text`, as itself (not `…/pull/12` inside `…/pull/123`).
pub fn names_pr(text: &str, url: &str) -> bool {
    !url.is_empty()
        && text.match_indices(url).any(|(i, _)| {
            let next = text[i + url.len()..].chars().next();
            !next.is_some_and(|c| c.is_ascii_alphanumeric())
        })
}

/// The ticket among YouTrack's search results whose description links the PR.
pub fn linked_on_youtrack(issues: &[Value], url: &str) -> Option<String> {
    issues.iter().find(|i| names_pr(i["description"].as_str().unwrap_or(""), url)).and_then(|i| i["idReadable"].as_str()).map(String::from)
}

/// The search for a PR's ticket on YouTrack: in its project, by its link.
pub fn search_query(cfg: &YoutrackCfg, pr: &Pr) -> String {
    format!("project: {{{}}} \"{}\"", cfg.project.trim(), pr.url)
}

/// What the tower asks of YouTrack and GitHub (`Real`; the tests' fake).
pub trait Remote {
    /// The project's id (by its short name or name).
    fn project_id(&self, project: &str) -> Result<String>;
    /// Issues matching a query: their `idReadable` and `description`.
    fn search(&self, query: &str) -> Result<Vec<Value>>;
    /// A new issue: its `idReadable`.
    fn create(&self, body: &Value) -> Result<String>;
    fn delete(&self, id: &str) -> Result<()>;
    /// A PR's comments: (id, body).
    fn comments(&self, repo: &str, number: u64) -> Result<Vec<(u64, String)>>;
    /// A new comment on a PR: its id.
    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<u64>;
    fn delete_comment(&self, repo: &str, id: u64) -> Result<()>;
}

/// The PR's ticket: found (on the PR, else on YouTrack) or made. Never a
/// second one: a search that fails makes none.
pub fn ensure(pr: &Pr, cfg: &YoutrackCfg, remote: &dyn Remote, changes: &str, now: u64) -> Result<Ticket> {
    if let Some(t) = pr.ticket.as_ref().filter(|t| !t.id.is_empty()) {
        return Ok(t.clone());
    }
    let base = base(cfg);
    let url_of = |id: &str| format!("{base}/issue/{id}");
    if cfg.link_pr
        && let Some((t, _)) = linked_in(&remote.comments(&pr.repo, pr.number).context("reading the PR's comments")?)
    {
        return Ok(Ticket { made_ms: now, ..t });
    }
    let found = remote.search(&search_query(cfg, pr)).context("looking for its ticket on YouTrack")?;
    if let Some(id) = linked_on_youtrack(&found, &pr.url) {
        return Ok(Ticket { url: url_of(&id), id, made_ms: now, how: "found on YouTrack".into(), comment: 0 });
    }
    let project = remote.project_id(&cfg.project).context("finding the project")?;
    let id = remote.create(&issue_body(cfg, &project, pr, changes)).context("making the ticket")?;
    Ok(Ticket { url: url_of(&id), id, made_ms: now, how: "made".into(), comment: 0 })
}

/// The PR's comment linking its ticket: the one there, else a new one. Its id.
pub fn link(pr: &Pr, t: &Ticket, remote: &dyn Remote) -> Result<u64> {
    if let Some((_, cid)) = linked_in(&remote.comments(&pr.repo, pr.number)?).filter(|(found, _)| found.id == t.id) {
        return Ok(cid);
    }
    remote.comment(&pr.repo, pr.number, &comment_text(t))
}

// ---- the token ----

fn security() -> &'static str {
    "/usr/bin/security"
}

/// The token: `HERDER_TOWER_YOUTRACK_TOKEN`, else the Keychain's. Never logged.
pub fn token() -> Option<String> {
    if let Some(t) = std::env::var("HERDER_TOWER_YOUTRACK_TOKEN").ok().filter(|t| !t.trim().is_empty()) {
        return Some(t.trim().to_string());
    }
    let out = Command::new(security()).args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT, "-w"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !t.is_empty()).then_some(t)
}

/// A token that can go into a header and a `security -i` line as it is.
pub fn token_ok(t: &str) -> bool {
    !t.is_empty() && t.len() < 4096 && t.chars().all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' && c != '\'')
}

/// Keeps the token in the Keychain, replacing the one there. Handed to
/// `security -i` on its stdin: never in a process's arguments.
pub fn store_token(t: &str) -> Result<()> {
    if !token_ok(t) {
        bail!("that doesn't look like a YouTrack token (one line, no spaces or quotes)");
    }
    let mut child = Command::new(security()).arg("-i").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().context("running security")?;
    if let Some(mut stdin) = child.stdin.take() {
        writeln!(stdin, "add-generic-password -U -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -l \"Herder Tower YouTrack token\" -w \"{t}\"")?;
    }
    let out = child.wait_with_output()?;
    let err = String::from_utf8_lossy(&out.stderr).replace(t, "<token>");
    if !out.status.success() || err.to_lowercase().contains("error") {
        bail!("the Keychain refused it: {}", err.trim());
    }
    Ok(())
}

/// Takes the token out of the Keychain.
pub fn forget_token() -> Result<bool> {
    let st = Command::new(security()).args(["delete-generic-password", "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status()?;
    Ok(st.success())
}

// ---- YouTrack and GitHub, for real ----

/// `%`-encoded for a URL's query.
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// A value for curl's config file, quoted.
fn curl_quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// YouTrack's REST API through curl, and GitHub through gh.
pub struct Real {
    pub base: String,
    pub token: String,
    pub gh: PathBuf,
}

impl Real {
    pub fn new(cfg: &YoutrackCfg, token: String) -> Real {
        Real { base: base(cfg), token, gh: crate::prs::gh_bin() }
    }

    /// One call: `path` under `/api/`, its JSON answer. The token goes in a
    /// header given on curl's stdin; a body, from a file of its own.
    pub fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = format!("{}/api/{}", self.base, path.trim_start_matches('/'));
        let file = match body {
            Some(b) => {
                let p = std::env::temp_dir().join(format!("tower-youtrack-{}-{}.json", std::process::id(), crate::history::now_ms()));
                std::fs::write(&p, serde_json::to_vec(b)?)?;
                Some(p)
            }
            None => None,
        };
        let mut cfg = format!(
            "url = {}\nrequest = {}\nheader = {}\nheader = \"Accept: application/json\"\nsilent\nshow-error\nmax-time = 30\nwrite-out = \"\\n%{{http_code}}\"\n",
            curl_quoted(&url),
            curl_quoted(method),
            curl_quoted(&format!("Authorization: Bearer {}", self.token)),
        );
        if let Some(p) = &file {
            cfg.push_str(&format!("header = \"Content-Type: application/json\"\ndata-binary = {}\n", curl_quoted(&format!("@{}", p.display()))));
        }
        let run = || -> Result<std::process::Output> {
            let mut child = Command::new("/usr/bin/curl").args(["-K", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().context("running curl")?;
            child.stdin.take().context("curl's stdin")?.write_all(cfg.as_bytes())?;
            Ok(child.wait_with_output()?)
        };
        let out = run();
        if let Some(p) = &file {
            let _ = std::fs::remove_file(p);
        }
        let out = out?;
        let text = String::from_utf8_lossy(&out.stdout).replace(&self.token, "<token>");
        let (answer, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
        let code: u16 = code.trim().parse().unwrap_or(0);
        if !out.status.success() || code == 0 {
            bail!("couldn't reach YouTrack: {}", String::from_utf8_lossy(&out.stderr).replace(&self.token, "<token>").trim());
        }
        let v: Value = if answer.trim().is_empty() { Value::Null } else { serde_json::from_str(answer).unwrap_or(Value::String(answer.trim().to_string())) };
        if code >= 400 {
            let why = v["error_description"].as_str().or(v["error"].as_str()).map(String::from).unwrap_or_else(|| answer.chars().take(200).collect());
            bail!("YouTrack said {code}: {why}");
        }
        Ok(v)
    }

    /// Who the token is (`login`).
    pub fn me(&self) -> Result<String> {
        let v = self.call("GET", "users/me?fields=login,name", None)?;
        Ok(v["login"].as_str().unwrap_or("").to_string())
    }

    /// The types the project's `Type` field allows (empty: it has none, or they can't be read).
    pub fn types(&self, project_id: &str) -> Vec<String> {
        let Ok(v) = self.call("GET", &format!("admin/projects/{project_id}/customFields?fields=field(name),bundle(values(name))&$top=100"), None) else { return vec![] };
        v.as_array()
            .into_iter()
            .flatten()
            .find(|f| f["field"]["name"] == "Type")
            .and_then(|f| f["bundle"]["values"].as_array())
            .map(|vs| vs.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect())
            .unwrap_or_default()
    }

    fn gh(&self, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.gh).args(args).stdin(Stdio::null()).output().with_context(|| format!("running {}", self.gh.display()))?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Remote for Real {
    fn project_id(&self, project: &str) -> Result<String> {
        let v = self.call("GET", "admin/projects?fields=id,shortName,name&$top=1000", None)?;
        let p = project.trim();
        v.as_array()
            .into_iter()
            .flatten()
            .find(|x| x["shortName"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(p)) || x["name"].as_str().is_some_and(|s| s.eq_ignore_ascii_case(p)))
            .and_then(|x| x["id"].as_str())
            .map(String::from)
            .with_context(|| format!("YouTrack has no project {p} this token can see"))
    }

    fn search(&self, query: &str) -> Result<Vec<Value>> {
        let v = self.call("GET", &format!("issues?query={}&fields=idReadable,description&$top=50", encode(query)), None)?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    fn create(&self, body: &Value) -> Result<String> {
        let v = self.call("POST", "issues?fields=id,idReadable", Some(body))?;
        v["idReadable"].as_str().filter(|s| !s.is_empty()).map(String::from).context("YouTrack made it but didn't say its id")
    }

    fn delete(&self, id: &str) -> Result<()> {
        self.call("DELETE", &format!("issues/{}", encode(id)), None).map(|_| ())
    }

    fn comments(&self, repo: &str, number: u64) -> Result<Vec<(u64, String)>> {
        let out = self.gh(&["api", "--paginate", &format!("repos/{repo}/issues/{number}/comments"), "--jq", ".[] | {id, body}"])?;
        Ok(out.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| Some((v["id"].as_u64()?, v["body"].as_str().unwrap_or("").to_string()))).collect())
    }

    fn comment(&self, repo: &str, number: u64, body: &str) -> Result<u64> {
        let out = self.gh(&["api", "-X", "POST", &format!("repos/{repo}/issues/{number}/comments"), "-f", &format!("body={body}"), "--jq", ".id"])?;
        out.trim().parse().context("GitHub didn't say the comment's id")
    }

    fn delete_comment(&self, repo: &str, id: u64) -> Result<()> {
        self.gh(&["api", "-X", "DELETE", &format!("repos/{repo}/issues/comments/{id}")]).map(|_| ())
    }
}

/// The PR whose ticket is `id`.
pub fn pr_of_ticket<'a>(prs: &'a [Pr], id: &str) -> Option<&'a Pr> {
    prs.iter().find(|p| p.ticket.as_ref().is_some_and(|t| t.id.eq_ignore_ascii_case(id.trim())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn cfg() -> YoutrackCfg {
        YoutrackCfg { enabled: true, url: "https://acme.youtrack.cloud/".into(), project: "EYZ".into(), issue_type: "Task".into(), ..Default::default() }
    }

    fn pr(repo: &str, n: u64) -> Pr {
        Pr {
            repo: repo.into(),
            number: n,
            url: format!("https://github.com/{repo}/pull/{n}"),
            title: "Support staff see open gaps".into(),
            state: "open".into(),
            base: "dev".into(),
            head: "gaps".into(),
            session: "t1cw".into(),
            name: "gaps-fix".into(),
            space: Some("support".into()),
            found_ms: 1_000,
            body: Some("Adds an open-gaps list to the support inbox.\n\nTested on staging.".into()),
            ..Default::default()
        }
    }

    /// YouTrack and GitHub, in memory: what was made, what was said.
    #[derive(Default)]
    struct Fake {
        issues: RefCell<Vec<Value>>,
        /// (PR number, comment id, body).
        comments: RefCell<Vec<(u64, u64, String)>>,
        created: RefCell<u32>,
        search_fails: bool,
        comments_fail: bool,
    }

    impl Remote for Fake {
        fn project_id(&self, project: &str) -> Result<String> {
            if project == "EYZ" { Ok("0-7".into()) } else { bail!("no project") }
        }
        fn search(&self, _: &str) -> Result<Vec<Value>> {
            if self.search_fails {
                bail!("YouTrack said 503");
            }
            Ok(self.issues.borrow().clone())
        }
        fn create(&self, body: &Value) -> Result<String> {
            *self.created.borrow_mut() += 1;
            let id = format!("EYZ-{}", 10 + self.issues.borrow().len());
            self.issues.borrow_mut().push(json!({ "idReadable": id, "description": body["description"], "summary": body["summary"] }));
            Ok(id)
        }
        fn delete(&self, id: &str) -> Result<()> {
            self.issues.borrow_mut().retain(|i| i["idReadable"] != id);
            Ok(())
        }
        fn comments(&self, _: &str, n: u64) -> Result<Vec<(u64, String)>> {
            if self.comments_fail {
                bail!("gh: HTTP 502");
            }
            Ok(self.comments.borrow().iter().filter(|c| c.0 == n).map(|c| (c.1, c.2.clone())).collect())
        }
        fn comment(&self, _: &str, n: u64, body: &str) -> Result<u64> {
            let id = 500 + self.comments.borrow().len() as u64;
            self.comments.borrow_mut().push((n, id, body.to_string()));
            Ok(id)
        }
        fn delete_comment(&self, _: &str, id: u64) -> Result<()> {
            self.comments.borrow_mut().retain(|c| c.1 != id);
            Ok(())
        }
    }

    #[test]
    fn an_open_eyzhub_pr_gets_a_ticket_and_nothing_else_does() {
        let c = cfg();
        let now = 2_000;
        assert_eq!(need(&pr("eyzhub/eyzflow", 1), &c, now), Some(Need::Ticket));
        // Its owner, however it's written.
        assert_eq!(need(&pr("EyzHub/eyzflow", 1), &c, now), Some(Need::Ticket));
        // Someone else's repo, or a PR merged or closed before it got one: never.
        assert_eq!(need(&pr("hasangilak/herder-tower", 1), &c, now), None);
        assert_eq!(need(&Pr { state: "merged".into(), ..pr("eyzhub/eyzflow", 1) }, &c, now), None);
        assert_eq!(need(&Pr { state: "closed".into(), ..pr("eyzhub/eyzflow", 1) }, &c, now), None);
        // A draft is a PR opened: it gets one.
        assert_eq!(need(&Pr { draft: true, ..pr("eyzhub/eyzflow", 1) }, &c, now), Some(Need::Ticket));
        // Its description not looked at yet: it waits for it, five minutes at most.
        let unread = Pr { body: None, ..pr("eyzhub/eyzflow", 1) };
        assert_eq!(need(&unread, &c, now), None);
        assert_eq!(need(&unread, &c, 1_000 + WAIT_FOR_BODY_MS), Some(Need::Ticket));
        // Once it has one: nothing more, but the link back when it isn't there yet.
        let t = Ticket { id: "EYZ-3".into(), url: "u".into(), ..Default::default() };
        assert_eq!(need(&Pr { ticket: Some(Ticket { comment: 9, ..t.clone() }), ..pr("eyzhub/eyzflow", 1) }, &c, now), None);
        assert_eq!(need(&Pr { ticket: Some(t.clone()), ..pr("eyzhub/eyzflow", 1) }, &c, now), Some(Need::Link));
        assert_eq!(need(&Pr { ticket: Some(t), ..pr("eyzhub/eyzflow", 1) }, &YoutrackCfg { link_pr: false, ..c.clone() }, now), None);
        // The owners are a setting.
        assert_eq!(need(&pr("acme/x", 1), &YoutrackCfg { github_owners: vec!["acme".into()], ..c }, now), Some(Need::Ticket));
        // What's missing before any is made, in words.
        assert_eq!(missing(&cfg(), true), None);
        assert_eq!(missing(&YoutrackCfg::default(), false).unwrap().matches("tower youtrack").count(), 3);
        assert_eq!(base(&YoutrackCfg { url: "https://x.youtrack.cloud/api/".into(), ..cfg() }), "https://x.youtrack.cloud");
    }

    #[test]
    fn a_ticket_holds_the_title_link_repo_space_session_and_what_it_changes() {
        let fake = Fake::default();
        let p = pr("eyzhub/eyz-customer-support", 42);
        let t = ensure(&p, &cfg(), &fake, &what_it_changes(&p), 5).unwrap();
        assert_eq!((t.id.as_str(), t.url.as_str(), t.how.as_str()), ("EYZ-10", "https://acme.youtrack.cloud/issue/EYZ-10", "made"));
        let issue = fake.issues.borrow()[0].clone();
        assert_eq!(issue["summary"], "Support staff see open gaps");
        let d = issue["description"].as_str().unwrap();
        for part in ["[eyzhub/eyz-customer-support#42](https://github.com/eyzhub/eyz-customer-support/pull/42)", "**Repo:** eyzhub/eyz-customer-support", "`gaps` → `dev`", "space **support**", "session **gaps-fix** (t1cw)", "Adds an open-gaps list to the support inbox."] {
            assert!(d.contains(part), "{part} in {d}");
        }
        assert!(!d.contains("Tested on staging"), "the opening paragraph only: {d}");
        // Its type, when one is set.
        let body = issue_body(&cfg(), "0-7", &p, "x");
        assert_eq!((body["project"]["id"].as_str(), body["customFields"][0]["value"]["name"].as_str()), (Some("0-7"), Some("Task")));
        assert!(issue_body(&YoutrackCfg { issue_type: String::new(), ..cfg() }, "0-7", &p, "x").get("customFields").is_none());
        // The merge card's summary, when it is of the description as it is.
        let body = p.body.clone().unwrap();
        let summed = Pr { described: Some(crate::describe::Described { summary: "Support sees which gaps are open.".into(), of: crate::describe::hash(&body) }), ..p.clone() };
        assert_eq!(what_it_changes(&summed), "Support sees which gaps are open.");
        let stale = Pr { described: Some(crate::describe::Described { summary: "old".into(), of: 1 }), ..p.clone() };
        assert_eq!(what_it_changes(&stale), "Adds an open-gaps list to the support inbox.");
        // No description: it says so.
        assert!(description(&Pr { body: Some(String::new()), ..p }, "").contains("The PR has no description yet."));
        // Linked back: the PR's comment carries its link and its mark.
        assert_eq!(link(&pr("eyzhub/eyz-customer-support", 42), &t, &fake).unwrap(), 500);
        let (said, _) = linked_in(&fake.comments("eyzhub/eyz-customer-support", 42).unwrap()).unwrap();
        assert_eq!((said.id.as_str(), said.url.as_str()), ("EYZ-10", "https://acme.youtrack.cloud/issue/EYZ-10"));
        assert!(fake.comments.borrow()[0].2.starts_with("YouTrack ticket: [EYZ-10](https://acme.youtrack.cloud/issue/EYZ-10)"));
    }

    #[test]
    fn a_pr_never_gets_a_second_ticket() {
        let c = cfg();
        let fake = Fake::default();
        let p = pr("eyzhub/eyzflow", 7);
        let t = ensure(&p, &c, &fake, "x", 5).unwrap();
        // Kept with the PR: asked again, the same one, nothing asked of YouTrack.
        let kept = Pr { ticket: Some(t.clone()), ..p.clone() };
        assert_eq!(ensure(&kept, &c, &Fake { search_fails: true, comments_fail: true, ..Default::default() }, "x", 6).unwrap(), t);
        assert_eq!(due(&[Pr { ticket: Some(Ticket { comment: 1, ..t.clone() }), ..p.clone() }], &c, 9), vec![]);
        // Made but not kept (the daemon stopped in between): found on YouTrack by the PR's link, not made again.
        let again = ensure(&p, &c, &fake, "x", 7).unwrap();
        assert_eq!((again.id.as_str(), again.how.as_str()), ("EYZ-10", "found on YouTrack"));
        assert_eq!(*fake.created.borrow(), 1);
        // Linked on the PR: found there first.
        link(&p, &t, &fake).unwrap();
        let on_pr = ensure(&p, &c, &Fake { comments: RefCell::new(fake.comments.borrow().clone()), ..Default::default() }, "x", 8).unwrap();
        assert_eq!((on_pr.id.as_str(), on_pr.how.as_str(), on_pr.comment), ("EYZ-10", "found on the PR", 500));
        // Its link isn't commented twice.
        assert_eq!(link(&p, &t, &fake).unwrap(), 500);
        assert_eq!(fake.comments.borrow().len(), 1);
        // YouTrack can't be searched, or the PR's comments read: none is made.
        let down = Fake { search_fails: true, ..Default::default() };
        assert!(ensure(&p, &c, &down, "x", 9).is_err());
        let gh_down = Fake { comments_fail: true, ..Default::default() };
        assert!(ensure(&p, &c, &gh_down, "x", 9).is_err());
        assert_eq!((*down.created.borrow(), *gh_down.created.borrow()), (0, 0));
        // Another PR whose link starts with this one's (#7 and #70) is another PR.
        let p70 = pr("eyzhub/eyzflow", 70);
        assert!(!names_pr("see https://github.com/eyzhub/eyzflow/pull/70)", &p.url));
        assert!(names_pr("[x](https://github.com/eyzhub/eyzflow/pull/7)", &p.url));
        let t70 = ensure(&p70, &c, &fake, "x", 10).unwrap();
        assert_eq!((t70.id.as_str(), t70.how.as_str()), ("EYZ-11", "made"));
        // A ticket's id finds its PR (what `tower youtrack remove` takes back).
        let all = [kept, Pr { ticket: Some(t70), ..p70 }];
        assert_eq!(pr_of_ticket(&all, "EYZ-11").map(|p| p.number), Some(70));
        assert_eq!(pr_of_ticket(&all, "eyz-10").map(|p| p.number), Some(7));
    }

    #[test]
    fn the_token_never_reaches_an_argument_or_a_log() {
        assert!(token_ok("perm:aGFzc2Fu.NDYtMQ==.abcDEF123"));
        for bad in ["", "two words", "a\"quote", "back\\slash", "new\nline"] {
            assert!(!token_ok(bad), "{bad:?}");
        }
        // The query is encoded; curl's config quotes what it is given.
        assert_eq!(encode("project: {EYZ} \"https://github.com/a/b/pull/1\""), "project%3A%20%7BEYZ%7D%20%22https%3A%2F%2Fgithub.com%2Fa%2Fb%2Fpull%2F1%22");
        assert_eq!(curl_quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        // A call that fails says why, without the token (a closed port: curl can't connect).
        let r = Real { base: "http://127.0.0.1:9".into(), token: "perm:secret-xyz".into(), gh: "gh".into() };
        let e = format!("{:#}", r.call("GET", "users/me", None).unwrap_err());
        assert!(!e.contains("secret-xyz") && e.contains("couldn't reach YouTrack"), "{e}");
    }

    /// The calls as curl makes them, against a server that echoes what it
    /// got (`HERDER_TOWER_YOUTRACK_ECHO=http://127.0.0.1:<port>`).
    #[test]
    #[ignore]
    fn calls_against_an_echo_server() {
        let base = std::env::var("HERDER_TOWER_YOUTRACK_ECHO").expect("HERDER_TOWER_YOUTRACK_ECHO");
        let r = Real { base, token: "perm:echo-test.TOKEN==".into(), gh: "gh".into() };
        let v = r.call("GET", "issues?query=a%20b&fields=idReadable", None).unwrap();
        assert_eq!((v["method"].as_str(), v["path"].as_str(), v["auth"].as_str()), (Some("GET"), Some("/api/issues?query=a%20b&fields=idReadable"), Some("Bearer <token>")), "sent, and masked in what came back");
        let id = r.create(&json!({ "summary": "Quote \" and backslash \\ and ünïcode" })).unwrap();
        assert_eq!(id, "T-1");
        let v = r.call("POST", "issues", Some(&json!({ "summary": "Quote \" and ünïcode" }))).unwrap();
        assert_eq!(v["ctype"], "application/json");
        assert_eq!(serde_json::from_str::<Value>(v["body"].as_str().unwrap()).unwrap()["summary"], "Quote \" and ünïcode");
        let e = format!("{:#}", r.call("GET", "missing", None).unwrap_err());
        assert_eq!(e, "YouTrack said 404: Entity not found");
        assert!(r.call("DELETE", "issues/T-1", None).is_ok());
    }
}
