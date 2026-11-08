//! The tower's main chat thread, the same one on every screen. It lives in
//! the history (`history.jsonl`, append-only, kept across restarts): every
//! request, from the Mac or a phone, every answer, the screenshots sessions
//! send, and the news the panel shows between them. This is that thread as
//! messages, for the phone API (`GET /v1/chat`, the `chat` event), with
//! exactly the panel's rules (`orb::panel::chat_items`, `gallery::chat_rows`).
//!
//! A message's id is `h<line>`, its first history line: stable, since the
//! history is only ever appended to.

use std::collections::HashSet;
use std::path::Path;

use serde_json::{Value, json};

use crate::history::Entry;

/// Screenshots a session sends this close together make one card (as the panel's).
const ONE_GO_MS: u64 = 2 * 60 * 1000;

/// The history with each entry's line number (1-based). Lines that don't
/// parse keep their number, so ids never shift.
pub fn load() -> Vec<(usize, Entry)> {
    let Ok(text) = std::fs::read_to_string(crate::history::path()) else { return vec![] };
    parse(&text)
}

pub fn parse(text: &str) -> Vec<(usize, Entry)> {
    text.lines().enumerate().filter_map(|(i, l)| serde_json::from_str(l).ok().map(|e| (i + 1, e))).collect()
}

/// What the chat shows (the panel's `chat_items`): requests and answers, and what came in between.
pub fn shown(e: &Entry) -> bool {
    !e.held
        && match e.kind.as_str() {
            "request" | "answer" | "watch" | "alert" | "summary" | "answered" | "behavior" => true,
            "screenshot" => e.path.is_some(),
            "job" => matches!(e.status.as_deref(), Some("done" | "failed" | "error" | "exited" | "blocked")) && !e.text.is_empty(),
            _ => false,
        }
}

/// `/v1/attachments/<folder>/<name>` for a file under the attachments folder.
fn attachment_url(path: &str, root: &Path) -> Option<String> {
    let rel = Path::new(path).strip_prefix(root).ok()?;
    let parts: Vec<String> = rel.iter().map(|p| enc(&p.to_string_lossy())).collect();
    (parts.len() == 2).then(|| format!("/v1/attachments/{}/{}", parts[0], parts[1]))
}

fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// A message's attachments, numbered as its tokens: `[Image #1]` is `n` 1.
fn attachments(e: &Entry, root: &Path) -> Vec<Value> {
    e.attachments
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = Path::new(p).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let kind = match crate::daemon::lan::file_kind(&name, None, &[]) {
                crate::daemon::lan::FileKind::Image => "image",
                crate::daemon::lan::FileKind::Video => "video",
                crate::daemon::lan::FileKind::Other => "file",
            };
            // Pictures can be fetched; other files are named.
            let url = if kind == "image" { attachment_url(p, root) } else { None };
            json!({ "n": i + 1, "kind": kind, "name": name, "path": p, "url": url })
        })
        .collect()
}

fn shot(e: &Entry) -> Value {
    json!({
        "id": e.id, "caption": e.caption.clone().unwrap_or_default(), "session": e.session, "name": e.name,
        "account": e.account, "at": e.at, "url": e.id.as_ref().map(|id| format!("/v1/screenshots/{id}")),
    })
}

/// The line the panel shows for news (its `event_line`).
fn event_text(e: &Entry) -> String {
    let who = e.name.clone().or_else(|| e.session.clone()).unwrap_or_default();
    match (e.kind.as_str(), e.status.as_deref()) {
        ("job", Some("done")) => format!("{who} finished: {}", e.text),
        ("job", Some("blocked")) => format!("{who} needs you: {}", e.text),
        ("job", _) => format!("{who}: {}", e.text),
        ("alert", _) => e.text.clone(),
        // The tower answered a session's question itself.
        ("answered", _) => format!("{who}: {}", e.text),
        ("behavior", _) => e.text.clone(),
        ("summary", _) => format!("{who}: {}", e.text),
        _ => {
            let mut t = e.text.clone();
            for cut in [" Read its screen and tell me what happened, briefly.", " Tell me briefly.", " Ask me before you answer it."] {
                t = t.replace(cut, "");
            }
            t
        }
    }
}

/// The thread, oldest first. `root`: the attachments folder (for picture URLs).
///
/// Each message: `id`, `at`, `role` (`user`, `tower`, `screenshots`,
/// `event`), `kind` (the history's), `text`, `source` (`mac`, `voice`,
/// `phone:<device>` for the user; `tower` otherwise), `attachments`,
/// `screenshots`, and for news and cards `session`, `name`, `status`.
pub fn thread(lines: &[(usize, Entry)], root: &Path) -> Vec<Value> {
    let by_id = |id: &str| lines.iter().map(|(_, e)| e).find(|e| e.kind == "screenshot" && e.id.as_deref() == Some(id));
    // Screenshots the tower attached to a message show under it, not on their own.
    let attached: HashSet<&str> = lines.iter().flat_map(|(_, e)| e.shots.iter().map(String::as_str)).collect();
    let mut out: Vec<Value> = vec![];
    // The open screenshot card: (its index in `out`, its first entry).
    let mut card: Option<(usize, &Entry)> = None;
    for (n, e) in lines.iter().filter(|(_, e)| shown(e)) {
        if e.kind == "screenshot" {
            if e.id.as_deref().is_some_and(|id| attached.contains(id)) {
                continue;
            }
            // Sent by the same session within two minutes of the card's first: the same card.
            if let Some((i, first)) = card
                && i + 1 == out.len()
                && first.session == e.session
                && e.at.saturating_sub(first.at) <= ONE_GO_MS
            {
                if let Some(list) = out[i]["screenshots"].as_array_mut() {
                    list.push(shot(e));
                }
                out[i]["updated_at"] = json!(e.at);
                continue;
            }
            out.push(json!({
                "id": format!("h{n}"), "at": e.at, "updated_at": e.at, "role": "screenshots", "kind": "screenshot",
                "text": "", "source": "tower", "attachments": [], "screenshots": [shot(e)],
                "session": e.session, "name": e.name,
            }));
            card = Some((out.len() - 1, e));
            continue;
        }
        let (role, text, source) = match e.kind.as_str() {
            "request" => ("user", e.text.clone(), e.source.clone().unwrap_or_else(|| "mac".into())),
            "answer" => ("tower", e.text.clone(), "tower".into()),
            _ => ("event", event_text(e), "tower".into()),
        };
        let shots: Vec<Value> = e.shots.iter().filter_map(|id| by_id(id)).map(shot).collect();
        let mut m = json!({
            "id": format!("h{n}"), "at": e.at, "updated_at": e.at, "role": role, "kind": e.kind, "text": text, "source": source,
            "attachments": attachments(e, root), "screenshots": shots,
        });
        if e.kind == "behavior" {
            m["diff"] = json!(e.diff);
        }
        if role == "event" {
            m["session"] = json!(e.session);
            m["name"] = json!(e.name);
            m["status"] = json!(e.status);
        }
        out.push(m);
    }
    out
}

/// A page of the thread: up to `limit` messages before message `before`
/// (newest-last), and whether older ones remain.
pub fn page(all: &[Value], before: Option<&str>, limit: usize) -> (Vec<Value>, bool) {
    let line = |m: &Value| m["id"].as_str().and_then(|i| i.strip_prefix('h')).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
    let cut = before.and_then(|b| b.strip_prefix('h')).and_then(|n| n.parse::<usize>().ok()).unwrap_or(usize::MAX);
    let older: Vec<&Value> = all.iter().filter(|m| line(m) < cut).collect();
    let start = older.len().saturating_sub(limit.max(1));
    (older[start..].iter().map(|m| (*m).clone()).collect(), start > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(kind: &str, at: u64, text: &str) -> Entry {
        Entry { at, kind: kind.into(), text: text.into(), ..Default::default() }
    }

    fn shot_entry(at: u64, id: &str, session: &str) -> Entry {
        Entry {
            at,
            kind: "screenshot".into(),
            id: Some(id.into()),
            session: Some(session.into()),
            caption: Some(format!("caption {id}")),
            path: Some(format!("/a/screenshots/{id}")),
            ..Default::default()
        }
    }

    #[test]
    fn one_thread_like_the_panel() {
        let root = Path::new("/home/me/attachments");
        let mut ask = e("request", 1, "[Image #1] [File #2: notes.pdf] compare these");
        ask.attachments = vec!["/home/me/attachments/17-phone/1-a b.png".into(), "/home/me/attachments/17-phone/2-notes.pdf".into()];
        ask.source = Some("phone:d_1".into());
        let mut answer = e("answer", 3, "Here it is.");
        answer.shots = vec!["s2.png".into()];
        let mut job = e("job", 9, "all tests pass");
        job.status = Some("done".into());
        job.name = Some("api".into());
        let mut working = e("job", 10, "started");
        working.status = Some("working".into());
        let mut held = e("alert", 11, "news from elsewhere");
        held.held = true;
        let lines: Vec<(usize, Entry)> = vec![
            ask,
            shot_entry(2, "s2.png", "t3"),
            answer,
            e("note", 4, "not in the chat"),
            shot_entry(5, "s5.png", "t3"),
            shot_entry(6, "s6.png", "t3"),
            shot_entry(5 + 3 * 60_000, "s7.png", "t3"),
            e("request", 8, "typed on the Mac"),
            job,
            working,
            held,
            e("watch", 12, "t3 (api) is now done. You had asked it: x. Read its screen and tell me what happened, briefly."),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, x)| (i + 1, x))
        .collect();
        let t = thread(&lines, root);
        let ids: Vec<&str> = t.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["h1", "h3", "h5", "h7", "h8", "h9", "h12"], "attached and held entries, notes and running jobs aren't messages");
        assert_eq!((t[0]["role"].as_str(), t[0]["source"].as_str()), (Some("user"), Some("phone:d_1")));
        assert_eq!(t[0]["attachments"][0]["url"], "/v1/attachments/17-phone/1-a%20b.png");
        assert_eq!((t[0]["attachments"][1]["kind"].as_str(), t[0]["attachments"][1]["url"].is_null()), (Some("file"), true));
        assert_eq!(t[1]["screenshots"][0]["url"], "/v1/screenshots/s2.png", "the tower's [shot:] shows under its reply");
        assert_eq!(t[2]["screenshots"].as_array().unwrap().len(), 2, "sent in one go: one card");
        assert_eq!(t[2]["updated_at"], 6);
        assert_eq!(t[3]["role"], "screenshots", "three minutes later: a card of its own");
        assert_eq!(t[4]["source"], "mac", "an old request without a source was typed on the Mac");
        assert_eq!(t[5]["text"], "api finished: all tests pass");
        assert_eq!(t[6]["text"], "t3 (api) is now done. You had asked it: x.");
    }

    #[test]
    fn pages_go_back_in_time() {
        let all: Vec<Value> = (1..=10).map(|n| json!({ "id": format!("h{}", n * 2) })).collect();
        let ids = |v: &[Value]| v.iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let (p, more) = page(&all, None, 3);
        assert_eq!((ids(&p), more), (vec!["h16".to_string(), "h18".into(), "h20".into()], true), "the newest, newest last");
        let (p, more) = page(&all, Some("h16"), 3);
        assert_eq!((ids(&p), more), (vec!["h10".to_string(), "h12".into(), "h14".into()], true));
        let (p, more) = page(&all, Some("h6"), 3);
        assert_eq!((ids(&p), more), (vec!["h2".to_string(), "h4".into()], false));
    }

    #[test]
    fn ids_follow_history_lines() {
        let text = "{\"at\":1,\"kind\":\"request\",\"text\":\"a\"}\nnot json\n{\"at\":2,\"kind\":\"answer\",\"text\":\"b\"}\n";
        let lines = parse(text);
        assert_eq!(lines.iter().map(|(n, _)| *n).collect::<Vec<_>>(), [1, 3], "a bad line keeps its number");
    }
}
