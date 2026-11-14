//! A session using a browser: Claude in Chrome (the user's own Chrome), the
//! built-in browser, a Playwright, Puppeteer or Chrome DevTools server, or a
//! headless browser it started.
//!
//! - **Its calls.** A browser tool is known by its MCP server's name
//!   (`kind_of_tool`): `mcp__claude-in-chrome__navigate` is Chrome. Each
//!   call is seen in the session's transcript (read on by the job watch,
//!   `daemon::running`) and, for a session started after the tower's
//!   PreToolUse hook was installed, the moment it is made. Its site: the
//!   last URL a call named, or a call's result showed (`host_of`).
//! - **Its processes.** A headless browser under the session's `claude`
//!   (`headless_of`): Playwright's Chromium, Puppeteer, agent-browser, a
//!   `--headless` Chrome.
//! - **Shown** while it uses one: "using Chrome: sooner.de, since 04:00";
//!   cleared `CLEAR_MS` after its last call (or when its browser process is
//!   gone).
//! - **Sharing.** Two sessions in the user's own browser at once (Chrome,
//!   the built-in browser) can fight over its tabs: each says with whom.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A browser use is over this long after its last call.
pub const CLEAR_MS: u64 = 90_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Claude in Chrome: the user's own Chrome.
    Chrome,
    /// The built-in browser (Claude_Browser).
    BuiltIn,
    Playwright,
    Puppeteer,
    DevTools,
    AgentBrowser,
    /// A headless browser of another kind.
    Headless,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Chrome => "Chrome",
            Kind::BuiltIn => "the built-in browser",
            Kind::Playwright => "Playwright",
            Kind::Puppeteer => "Puppeteer",
            Kind::DevTools => "Chrome DevTools",
            Kind::AgentBrowser => "agent-browser",
            Kind::Headless => "a headless browser",
        }
    }

    /// The user's own browser: two sessions in it at once get in each other's way.
    pub fn shared(self) -> bool {
        matches!(self, Kind::Chrome | Kind::BuiltIn)
    }
}

/// Which browser a tool drives, by its MCP server's name. None: no browser tool.
pub fn kind_of_tool(name: &str) -> Option<Kind> {
    let n = name.to_lowercase();
    if !n.starts_with("mcp__") {
        return None;
    }
    let server = n.trim_start_matches("mcp__").split("__").next().unwrap_or("");
    let whole = n.as_str();
    Some(if server.contains("claude-in-chrome") || server == "chrome" {
        Kind::Chrome
    } else if whole.contains("claude_browser") {
        Kind::BuiltIn
    } else if server.contains("playwright") {
        Kind::Playwright
    } else if server.contains("puppeteer") {
        Kind::Puppeteer
    } else if server.contains("chrome-devtools") || server.contains("chrome_devtools") || server.contains("devtools") {
        Kind::DevTools
    } else if server.contains("agent-browser") {
        Kind::AgentBrowser
    } else if server.contains("browser") {
        Kind::Headless
    } else {
        return None;
    })
}

/// The site of the first `http(s)://` URL in `text`: its host (with its port
/// for a local one: `localhost:3006`), without `www.`.
pub fn host_of(text: &str) -> Option<String> {
    let at = text.find("https://").or_else(|| text.find("http://"))?;
    let rest = &text[at..];
    let rest = &rest[rest.find("://")? + 3..];
    let end = rest.find(|c: char| matches!(c, '/' | '?' | '#' | '"' | '\'' | ' ' | ')' | '\\' | ',' | '\n' | ']' | '>')).unwrap_or(rest.len());
    let host = rest[..end].trim_end_matches('.');
    let host = host.rsplit('@').next().unwrap_or(host);
    let (name, port) = match host.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (host, None),
    };
    if name.is_empty() {
        return None;
    }
    let name = name.strip_prefix("www.").unwrap_or(name).to_lowercase();
    let local = matches!(name.as_str(), "localhost" | "127.0.0.1" | "0.0.0.0") || name.ends_with(".local");
    Some(match (port, local) {
        (Some(p), true) => format!("{name}:{p}"),
        _ => name,
    })
}

/// A run of calls to one browser.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub since_ms: u64,
    pub last_ms: u64,
    pub host: Option<String>,
}

/// What a session's calls say, by browser, as its transcript grows.
#[derive(Debug, Clone, Default)]
pub struct Calls {
    pub runs: BTreeMap<Kind, Run>,
    /// Browser calls whose result hasn't come yet, by tool use id.
    pending: BTreeMap<String, Kind>,
}

impl Calls {
    /// A call to `kind` at `at_ms`, naming `host` if it does: a new run when
    /// the last one is over (`CLEAR_MS` since its last call).
    pub fn call(&mut self, kind: Kind, at_ms: u64, host: Option<String>) {
        let r = self.runs.entry(kind).or_default();
        if r.last_ms == 0 || at_ms > r.last_ms + CLEAR_MS {
            r.since_ms = at_ms;
        }
        r.last_ms = r.last_ms.max(at_ms);
        if host.is_some() {
            r.host = host;
        }
    }

    /// Takes in more transcript lines.
    pub fn read(&mut self, lines: &str) {
        for line in lines.lines() {
            if !(line.contains("mcp__") || (!self.pending.is_empty() && line.contains("tool_result"))) {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            let at = v["timestamp"].as_str().and_then(crate::releases::ms_of_iso).unwrap_or(0);
            let Some(blocks) = v["message"]["content"].as_array() else { continue };
            for b in blocks {
                match b["type"].as_str() {
                    Some("tool_use") => {
                        let Some(kind) = b["name"].as_str().and_then(kind_of_tool) else { continue };
                        // The site a call names (`url`, or any URL in its input).
                        let host = b["input"]["url"].as_str().and_then(host_of).or_else(|| host_of(&b["input"].to_string()));
                        self.call(kind, at, host);
                        if let Some(id) = b["id"].as_str() {
                            self.pending.insert(id.to_string(), kind);
                        }
                    }
                    Some("tool_result") => {
                        let Some(kind) = b["tool_use_id"].as_str().and_then(|id| self.pending.remove(id)) else { continue };
                        let text = match &b["content"] {
                            Value::String(s) => s.clone(),
                            c => c.to_string(),
                        };
                        // The page it is on, as the result shows it (a tab's URL).
                        if let Some(h) = host_of(&text)
                            && let Some(r) = self.runs.get_mut(&kind)
                        {
                            r.host = Some(h);
                        }
                    }
                    _ => {}
                }
            }
        }
        // Keep the pending list short: a call whose result never came.
        if self.pending.len() > 200 {
            let cut: Vec<String> = self.pending.keys().take(self.pending.len() - 200).cloned().collect();
            for k in cut {
                self.pending.remove(&k);
            }
        }
    }
}

/// A headless browser among a session's processes (its commands): which.
pub fn headless_of(commands: &[&str]) -> Option<Kind> {
    let mut found = None;
    for c in commands {
        let l = c.to_lowercase();
        let browser = l.contains("chrome-headless-shell") || l.contains("--headless") || l.contains("chrome for testing") || l.contains("/chromium") || l.contains("headless_shell");
        let k = if l.contains("ms-playwright") || l.contains("playwright") {
            Some(Kind::Playwright)
        } else if l.contains("puppeteer") {
            Some(Kind::Puppeteer)
        } else if l.contains("agent-browser") {
            Some(Kind::AgentBrowser)
        } else if browser {
            Some(Kind::Headless)
        } else {
            None
        };
        // A browser process itself is the proof; a runner alone (`npx playwright test`) counts too.
        if let Some(k) = k {
            found = Some(match found {
                Some(Kind::Headless) | None => k,
                Some(f) => f,
            });
        }
    }
    found
}

/// A session's use of a browser, as shown.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Use {
    pub kind: Option<Kind>,
    /// "Chrome", "Playwright".
    pub browser: String,
    pub host: Option<String>,
    pub since_ms: u64,
    pub last_ms: u64,
    /// `tools` (its calls) or `process` (a browser it runs).
    pub via: String,
    /// Other sessions in the same browser of the user's now (addresses).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shared_with: Vec<String>,
    /// "using Chrome: sooner.de, since 04:00".
    pub line: String,
}

/// "using Chrome: sooner.de, since 04:00" (and with whom it shares it).
pub fn line(kind: Kind, host: Option<&str>, since_ms: u64, now_ms: u64, shared_with: &[String]) -> String {
    let at = host.map(|h| format!(": {h}")).unwrap_or_default();
    let mut s = format!("using {}{at}, since {}", kind.name(), crate::failover::when(since_ms, now_ms));
    if !shared_with.is_empty() {
        s += &format!("; shares {} with {}", kind.name(), shared_with.join(", "));
    }
    s
}

/// A session's uses now, from its calls and its processes (`headless`: a
/// browser it runs, and since when): those still on, the latest first.
pub fn uses(calls: &Calls, headless: Option<(Kind, u64)>, now_ms: u64) -> Vec<Use> {
    let mut out: Vec<Use> = calls
        .runs
        .iter()
        .filter(|(_, r)| r.last_ms > 0 && now_ms <= r.last_ms + CLEAR_MS)
        .map(|(k, r)| Use { kind: Some(*k), browser: k.name().into(), host: r.host.clone(), since_ms: r.since_ms, last_ms: r.last_ms, via: "tools".into(), ..Default::default() })
        .collect();
    if let Some((k, since)) = headless
        && !out.iter().any(|u| u.kind == Some(k))
    {
        out.push(Use { kind: Some(k), browser: k.name().into(), since_ms: since, last_ms: now_ms, via: "process".into(), ..Default::default() });
    }
    out.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
    out
}

/// Every session's uses, with who shares the user's own browser with whom,
/// and each one's line. Returns the pairs sharing one now (`kind`, addresses).
pub fn settle(all: &mut BTreeMap<String, Vec<Use>>, names: &BTreeMap<String, String>, now_ms: u64) -> Vec<(Kind, Vec<String>)> {
    let mut by_kind: BTreeMap<Kind, Vec<String>> = BTreeMap::new();
    for (address, uses) in all.iter() {
        for u in uses {
            if let Some(k) = u.kind.filter(|k| k.shared()) {
                by_kind.entry(k).or_default().push(address.clone());
            }
        }
    }
    let shared: Vec<(Kind, Vec<String>)> = by_kind.into_iter().filter(|(_, l)| l.len() > 1).collect();
    for (address, uses) in all.iter_mut() {
        for u in uses.iter_mut() {
            let Some(k) = u.kind else { continue };
            u.shared_with = shared
                .iter()
                .filter(|(sk, _)| *sk == k)
                .flat_map(|(_, l)| l.iter().filter(|a| *a != address).map(|a| names.get(a).map(|n| format!("{n} ({a})")).unwrap_or_else(|| a.clone())))
                .collect();
            u.line = line(k, u.host.as_deref(), u.since_ms, now_ms, &u.shared_with);
        }
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_794_625_000_000;

    /// `HERDER_TOWER_BROWSER_TRANSCRIPT=<a transcript> cargo test
    /// replay_a_transcripts_browser_calls -- --ignored --nocapture`: the runs
    /// of browser calls in it, as the tower reads them.
    #[test]
    #[ignore]
    fn replay_a_transcripts_browser_calls() {
        let text = std::fs::read_to_string(std::env::var("HERDER_TOWER_BROWSER_TRANSCRIPT").expect("a transcript")).unwrap();
        let mut c = Calls::default();
        c.read(&text);
        let now = crate::history::now_ms();
        for (k, r) in &c.runs {
            eprintln!("{}: {} | last call {} | still shown: {}", k.name(), line(*k, r.host.as_deref(), r.since_ms, now, &[]), crate::failover::when(r.last_ms, now), now <= r.last_ms + CLEAR_MS);
        }
    }

    #[test]
    fn a_browser_tool_is_known_by_its_server() {
        assert_eq!(kind_of_tool("mcp__claude-in-chrome__navigate"), Some(Kind::Chrome));
        assert_eq!(kind_of_tool("mcp__claude-in-chrome__browser_batch"), Some(Kind::Chrome));
        assert_eq!(kind_of_tool("mcp__Claude_Browser__click"), Some(Kind::BuiltIn));
        assert_eq!(kind_of_tool("mcp__remote-devices__Claude_Browser__navigate"), Some(Kind::BuiltIn));
        assert_eq!(kind_of_tool("mcp__playwright__browser_navigate"), Some(Kind::Playwright));
        assert_eq!(kind_of_tool("mcp__puppeteer__puppeteer_screenshot"), Some(Kind::Puppeteer));
        assert_eq!(kind_of_tool("mcp__chrome-devtools__take_snapshot"), Some(Kind::DevTools));
        for no in ["Bash", "WebFetch", "mcp__blender__execute_blender_code", "mcp__plugin_sentry_sentry__search_issues"] {
            assert_eq!(kind_of_tool(no), None, "{no}");
        }
    }

    #[test]
    fn the_site_is_the_urls_host() {
        assert_eq!(host_of("https://www.sooner.de/de/filme?x=1").as_deref(), Some("sooner.de"));
        assert_eq!(host_of(r#"{"url": "http://localhost:3006/de"}"#).as_deref(), Some("localhost:3006"));
        assert_eq!(host_of("Tab 1185358176: Sooner (https://sooner.de/de)").as_deref(), Some("sooner.de"));
        assert_eq!(host_of("https://app.example.com:8443/x").as_deref(), Some("app.example.com"));
        assert_eq!(host_of("no link here"), None);
    }

    #[test]
    fn calls_in_a_transcript_make_a_run_with_its_site_and_its_end() {
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-11-14T03:00:10.000Z","message":{"content":[{"type":"tool_use","id":"u1","name":"mcp__claude-in-chrome__tabs_context_mcp","input":{"createIfEmpty":true}}]}}"#,
            r#"{"type":"user","timestamp":"2026-11-14T03:00:11.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"u1","content":[{"type":"text","text":"Tabs: 1185358176 Sooner https://sooner.de/de"}]}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-14T03:00:46.720Z","message":{"content":[{"type":"tool_use","id":"u2","name":"mcp__claude-in-chrome__browser_batch","input":{"actions":[{"name":"computer"}]}}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-11-14T03:00:50.000Z","message":{"content":[{"type":"tool_use","id":"u3","name":"Bash","input":{"command":"ls"}}]}}"#,
        ]
        .join("\n");
        let mut c = Calls::default();
        c.read(&lines);
        let r = &c.runs[&Kind::Chrome];
        let start = crate::releases::ms_of_iso("2026-11-14T03:00:10.000Z").unwrap();
        assert_eq!((r.since_ms, r.host.as_deref()), (start, Some("sooner.de")), "its site from the tab the result showed");
        assert_eq!(r.last_ms, crate::releases::ms_of_iso("2026-11-14T03:00:46.720Z").unwrap());
        assert_eq!(c.runs.len(), 1, "Bash is no browser");
        // Shown while on, cleared after `CLEAR_MS`.
        let u = uses(&c, None, r.last_ms + 30_000);
        assert_eq!((u.len(), u[0].browser.as_str(), u[0].via.as_str()), (1, "Chrome", "tools"));
        assert!(uses(&c, None, r.last_ms + CLEAR_MS + 1).is_empty());
        // A call after a pause is a new run: its own since.
        let later = r.last_ms + 10 * 60_000;
        c.call(Kind::Chrome, later, None);
        assert_eq!(c.runs[&Kind::Chrome].since_ms, later);
        assert_eq!(c.runs[&Kind::Chrome].host.as_deref(), Some("sooner.de"), "a call that names no site keeps the last");
    }

    #[test]
    fn a_headless_browser_among_its_processes() {
        assert_eq!(headless_of(&["node node_modules/.bin/playwright test", "/Users/me/Library/Caches/ms-playwright/chromium_headless_shell-1187/chrome-mac/headless_shell --headless"]), Some(Kind::Playwright));
        assert_eq!(headless_of(&["node scripts/shot.js", "/x/.cache/puppeteer/chrome/mac-131/chrome --headless=new"]), Some(Kind::Puppeteer));
        assert_eq!(headless_of(&["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome --headless --dump-dom https://x"]), Some(Kind::Headless));
        assert_eq!(headless_of(&["agent-browser open https://x"]), Some(Kind::AgentBrowser));
        assert_eq!(headless_of(&["npm run dev", "node server.js"]), None);
    }

    #[test]
    fn two_sessions_in_the_users_chrome_share_it() {
        let u = |k: Kind, host: &str| Use { kind: Some(k), browser: k.name().into(), host: Some(host.into()), since_ms: NOW - 60_000, last_ms: NOW, via: "tools".into(), ..Default::default() };
        let mut all: BTreeMap<String, Vec<Use>> = BTreeMap::new();
        all.insert("t14w".into(), vec![u(Kind::Chrome, "sooner.de")]);
        all.insert("t15a".into(), vec![u(Kind::Chrome, "example.com")]);
        all.insert("t15b".into(), vec![u(Kind::Playwright, "localhost:3000")]);
        all.insert("t15c".into(), vec![u(Kind::Playwright, "localhost:3001")]);
        let names: BTreeMap<String, String> = [("t14w".to_string(), "sooner app web".to_string())].into();
        let shared = settle(&mut all, &names, NOW);
        assert_eq!(shared, [(Kind::Chrome, vec!["t14w".to_string(), "t15a".to_string()])], "headless browsers of their own aren't shared");
        assert_eq!(all["t15a"][0].shared_with, ["sooner app web (t14w)"]);
        assert!(all["t15a"][0].line.starts_with("using Chrome: example.com, since ") && all["t15a"][0].line.ends_with("; shares Chrome with sooner app web (t14w)"), "{}", all["t15a"][0].line);
        assert!(all["t15b"][0].shared_with.is_empty());
        assert!(all["t15b"][0].line.starts_with("using Playwright: localhost:3000, since "));
    }
}
