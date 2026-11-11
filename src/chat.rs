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

/// A history's text with each entry's line number (1-based). Lines that
/// don't parse keep their number, so ids never shift.
pub fn parse(text: &str) -> Vec<(usize, Entry)> {
    text.lines().enumerate().filter_map(|(i, l)| serde_json::from_str(l).ok().map(|e| (i + 1, e))).collect()
}

/// What the chat shows (the panel's `chat_items`): requests and answers, and
/// what came in between. Never what was only written down (`record`: at a
/// quiet level that is every finish, delivery and routine answer, and a
/// receipt nobody asked to hear), what the brain wrote on a note turn
/// (`note`), or what sessions wrote to each other (`peer`: their thread).
/// What the tower opened in answer to a request (`act`) is the receipt of
/// that request; one that answered none is `record`.
pub fn shown(e: &Entry) -> bool {
    !e.held
        && !e.record
        && match e.kind.as_str() {
            "request" | "answer" | "watch" | "alert" | "summary" | "answered" | "behavior" | "receipt" | "act" => true,
            "screenshot" => e.path.is_some(),
            // A PR a session opened: its chip.
            "pr" => e.id.is_some(),
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
        _ => report_text(&e.text),
    }
}

/// A report for the brain as the chat shows it: the news, without what it
/// tells the brain to do (read the screen, how to say it, that its space is
/// out of focus).
pub fn report_text(text: &str) -> String {
    let mut t = text.split(" This is from the space ").next().unwrap_or("").to_string();
    for cut in [
        " Read its screen and tell me what happened, briefly.",
        " Tell me what happened, briefly (read its screen only if that isn't enough).",
        " Tell the user plainly what they need to do (with the PR or link if there is one).",
        " Tell the user in one short line only if there's something more for them to do.",
        " Tell me briefly.",
        " Ask me before you answer it.",
        crate::tell::NOTE_TAIL,
    ] {
        t = t.replace(cut, "");
    }
    t
}

/// A request that waits this long for its answer has none coming: the
/// next answer is the next request's.
const UNANSWERED_MS: u64 = 30 * 60_000;

/// The space a request names in front (`In <space>: …`, as a space's view
/// sends it to the main tower while the space has no tower of its own),
/// among the spaces the tower keeps: the longest name that fits, any case.
pub fn asked_in(text: &str, spaces: &[String]) -> Option<String> {
    let rest = text.trim_start().strip_prefix("In ")?;
    spaces
        .iter()
        .filter(|s| !s.trim().is_empty())
        .filter(|s| rest.get(..s.len()).is_some_and(|head| head.eq_ignore_ascii_case(s)) && rest[s.len()..].starts_with(':'))
        .max_by_key(|s| s.len())
        .cloned()
}

/// The thread, oldest first. `root`: the attachments folder (for picture
/// URLs); `prs`: the PRs by key, as they are now (a chip shows its PR's
/// state, so it changes in place); `spaces`: the spaces the tower keeps
/// (for a request that names one in front, see `asked_in`).
///
/// Each message: `id`, `at`, `role` (`user`, `tower`, `screenshots`,
/// `event`), `kind` (the history's), `text`, `source` (`mac`, `voice`,
/// `phone:<device>` for the user; `tower` otherwise), `space`,
/// `attachments`, `screenshots`, and for news and cards `session`, `name`,
/// `status`; a PR chip (`role` `pr`) has `pr`, the PR as it is now
/// (`prs::Pr::view`).
///
/// `space`: for a request, the space in whose view it was asked (of that
/// space's own tower, or of the main one with the space named in front);
/// for an answer, the space of the request it answers; null for what was
/// asked of the tower itself. For anything else, where it comes from (its
/// session's space, a receipt's request's). See `in_conversation`.
#[cfg(test)]
pub fn thread_with(lines: &[(usize, Entry)], root: &Path, prs: &std::collections::BTreeMap<String, crate::prs::Pr>, spaces: &[String]) -> Vec<Value> {
    thread_of(lines, root, prs, spaces, &[])
}

/// An act as its history entry has it, when the tower no longer holds it:
/// what it was, and nothing to undo.
pub fn act_of_entry(e: &Entry) -> Value {
    let a = e.act.clone().unwrap_or_default();
    let undone = e.status.as_deref() == Some("undone");
    json!({
        "id": e.id, "kind": a["kind"], "at_ms": e.at, "by": a["by"], "space": e.space,
        "line": if undone { Value::Null } else { json!(e.text) },
        "sessions": a["sessions"], "assumed": a["assumed"],
        "request": a["request_at"].as_u64().map(|at| json!({ "at": at })),
        "state": if undone { "undone" } else { "done" },
        "undo": { "possible": false, "why": if undone { "it is undone already" } else { "it was opened too long ago to undo from here: tower undo <id>" } },
        "closing": if undone { json!(e.text) } else { Value::Null },
    })
}

/// `thread_with`, with the acts as the tower holds them now (`acts`: their
/// views, `crate::acts::Act::view`): what the tower opened in answer to a
/// request is one message (`role` `tower`, `kind` `act`) where it was
/// opened, with `act` as it is now, so it changes in place when more
/// sessions join it, when it can no longer be undone, and when it is.
pub fn thread_of(
    lines: &[(usize, Entry)],
    root: &Path,
    prs: &std::collections::BTreeMap<String, crate::prs::Pr>,
    spaces: &[String],
    acts: &[Value],
) -> Vec<Value> {
    // Each act's message, by its id.
    let mut act_at: std::collections::HashMap<String, usize> = Default::default();
    let by_id = |id: &str| lines.iter().map(|(_, e)| e).find(|e| e.kind == "screenshot" && e.id.as_deref() == Some(id));
    // Screenshots the tower attached to a message show under it, not on their own.
    let attached: HashSet<&str> = lines.iter().flat_map(|(_, e)| e.shots.iter().map(String::as_str)).collect();
    let mut out: Vec<Value> = vec![];
    // The open screenshot card: (its index in `out`, its first entry).
    let mut card: Option<(usize, &Entry)> = None;
    // What was asked of the main tower and has no answer yet, oldest first:
    // when, and the space it named in front.
    let mut waiting: std::collections::VecDeque<(u64, Option<String>)> = Default::default();
    // Each request's conversation, by when it was asked: an act answers one.
    let mut asked_at: std::collections::HashMap<u64, Option<String>> = Default::default();
    for (n, e) in lines.iter().filter(|(_, e)| shown(e)) {
        let own = e.space.clone().filter(|s| !s.is_empty());
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
                "text": "", "source": "tower", "space": own, "attachments": [], "screenshots": [shot(e)],
                "session": e.session, "name": e.name,
            }));
            card = Some((out.len() - 1, e));
            continue;
        }
        if e.kind == "act" {
            let id = e.id.clone().unwrap_or_default();
            let now = acts.iter().find(|a| a["id"].as_str() == Some(id.as_str())).cloned();
            // Whose conversation: the request it answered was asked there (of
            // that space's own tower, or of the main one with the space named in front).
            let asked_in = e.act.as_ref().and_then(|a| a["request_space"].as_str()).filter(|s| !s.is_empty()).map(String::from).or_else(|| {
                e.act.as_ref().and_then(|a| a["request_at"].as_u64()).and_then(|at| asked_at.get(&at).cloned().flatten())
            });
            let act = now.unwrap_or_else(|| act_of_entry(e));
            let text = match act["state"].as_str() {
                Some("undone") => act["closing"].as_str().or(act["line"].as_str()).unwrap_or(&e.text).to_string(),
                _ => act["line"].as_str().unwrap_or(&e.text).to_string(),
            };
            match act_at.get(&id) {
                // Said again (another session joined it, it was undone): the same message, as it is now.
                Some(&i) => {
                    out[i]["updated_at"] = json!(e.at);
                    if acts.iter().all(|a| a["id"].as_str() != Some(id.as_str())) {
                        // The tower forgot it: its last entry says how it ended.
                        let mut last = act_of_entry(e);
                        if last["line"].is_null() {
                            last["line"] = out[i]["act"]["line"].clone();
                        }
                        out[i]["text"] = json!(e.text);
                        out[i]["act"] = last;
                    }
                }
                None => {
                    act_at.insert(id, out.len());
                    out.push(json!({
                        "id": format!("h{n}"), "at": e.at, "updated_at": e.at, "role": "tower", "kind": "act", "text": text, "source": "tower",
                        "space": asked_in, "attachments": [], "screenshots": [], "act": act,
                    }));
                }
            }
            continue;
        }
        if e.kind == "pr" {
            let key = e.id.clone().unwrap_or_default();
            out.push(json!({
                "id": format!("h{n}"), "at": e.at, "updated_at": prs.get(&key).map(|p| p.checked_ms.max(e.at)).unwrap_or(e.at),
                "role": "pr", "kind": "pr", "text": e.text, "source": "tower", "attachments": [], "screenshots": [],
                "session": e.session, "name": e.name, "space": e.space,
                "pr": prs.get(&key).map(crate::prs::Pr::view).unwrap_or_else(|| json!({ "key": key, "title": e.text })),
            }));
            continue;
        }
        let (role, text, source) = match e.kind.as_str() {
            "request" => ("user", e.text.clone(), e.source.clone().unwrap_or_else(|| "mac".into())),
            // A receipt is the tower's reply to the request it closes.
            "answer" | "receipt" => ("tower", e.text.clone(), "tower".into()),
            _ => ("event", event_text(e), "tower".into()),
        };
        // A space's own tower's requests and answers carry its space. The
        // main tower's: a request may name a space in front, and the answer
        // to it is that space's too.
        let space = match (e.kind.as_str(), own) {
            ("request", None) => {
                waiting.retain(|(at, _)| e.at.saturating_sub(*at) < UNANSWERED_MS);
                let about = asked_in(&e.text, spaces);
                waiting.push_back((e.at, about.clone()));
                about
            }
            ("answer", None) => waiting.pop_front().and_then(|(_, about)| about),
            (_, own) => own,
        };
        if e.kind == "request" {
            asked_at.insert(e.at, space.clone());
        }
        let shots: Vec<Value> = e.shots.iter().filter_map(|id| by_id(id)).map(shot).collect();
        let mut m = json!({
            "id": format!("h{n}"), "at": e.at, "updated_at": e.at, "role": role, "kind": e.kind, "text": text, "source": source,
            "space": space, "attachments": attachments(e, root), "screenshots": shots,
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

/// Whether a message belongs to the conversation with a space's own tower
/// (`space`: its name, any case), or to the main one (None).
///
/// A request or an answer that has a `space` is that space's alone, and so
/// is what the tower opened in answer to one (`act`: its `space` is the
/// conversation it was asked in). A receipt that names a space is shown
/// there and in the main one (it is the reply to a request that may have
/// been asked in either). Everything else (news, cards, PR chips) is the
/// main one's.
pub fn in_conversation(m: &Value, space: Option<&str>) -> bool {
    let own = m["space"].as_str().filter(|s| !s.is_empty());
    let kind = m["kind"].as_str().unwrap_or("");
    match space {
        Some(s) => matches!(kind, "request" | "answer" | "receipt" | "act") && own.is_some_and(|o| o.eq_ignore_ascii_case(s.trim())),
        None => !(matches!(kind, "request" | "answer" | "act") && own.is_some()),
    }
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
    fn what_the_tower_opened_is_the_receipt_of_its_request() {
        use crate::acts::{Act, Asked, Assumed, Opened};
        let act = |sessions: &[(&str, &str)], asked_in: Option<&str>| Act {
            id: "r0123456789ab".into(),
            kind: "start".into(),
            at_ms: 2,
            by: "tower".into(),
            space: Some("M5 Pro".into()),
            sessions: sessions.iter().map(|(a, n)| Opened { address: a.to_string(), name: n.to_string(), ..Default::default() }).collect(),
            assumed: vec![Assumed { what: "account".into(), value: "orca".into(), why: "what \"second\" names".into() }],
            request: Some(Asked { at: 1, text: "open a session in M5 Pro on my second account".into(), space: asked_in.map(String::from) }),
            state: "done".into(),
            ..Default::default()
        };
        let one = act(&[("t9", "fix-login")], None);
        let two = act(&[("t9", "fix-login"), ("t10", "docs")], None);
        let mut undone = two.clone();
        (undone.state, undone.closing) = ("undone".into(), Some("Closed it. Nothing else changed.".into()));
        let at = |mut e: Entry, at: u64| {
            e.at = at;
            e
        };
        // For the record only: what answered no request.
        let quiet = Act { request: None, id: "r00000000000f".into(), ..one.clone() };
        let lines: Vec<(usize, Entry)> = vec![
            e("request", 1, "open a session in M5 Pro on my second account"),
            at(one.entry(), 2),
            at(two.entry(), 3),
            e("answer", 4, "Opened two sessions in M5 Pro."),
            at(quiet.entry(), 5),
            at(undone.entry(), 6),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, e)| (i + 1, e))
        .collect();
        // As the tower holds it now: one message where it was opened, with the act as it is.
        let t = thread_of(&lines[..4], Path::new("/a"), &Default::default(), &[], &[two.view()]);
        assert_eq!(t.iter().map(|m| (m["id"].as_str().unwrap(), m["role"].as_str().unwrap(), m["kind"].as_str().unwrap())).collect::<Vec<_>>(), [("h1", "user", "request"), ("h2", "tower", "act"), ("h4", "tower", "answer")]);
        assert_eq!((t[1]["text"].as_str(), t[1]["updated_at"].as_u64(), t[1]["space"].is_null()), (Some("Opened 2 sessions in M5 Pro: fix login and docs."), Some(3), true));
        assert_eq!(t[1]["act"], two.view(), "what was assumed, and whether it can be undone");
        assert_eq!((t[1]["act"]["undo"]["possible"].as_bool(), t[1]["act"]["assumed"][0]["value"].as_str()), (Some(true), Some("orca")));
        // Undone: the same message says how it closed.
        let t = thread_of(&lines, Path::new("/a"), &Default::default(), &[], &[undone.view()]);
        assert_eq!(t.len(), 3, "what answered no request is not a message");
        assert_eq!((t[1]["id"].as_str(), t[1]["text"].as_str(), t[1]["act"]["state"].as_str(), t[1]["updated_at"].as_u64()), (Some("h2"), Some("Closed it. Nothing else changed."), Some("undone"), Some(6)));
        // The tower no longer holds it (a restart long after): its entries say what it was, and nothing is offered.
        let t = thread_with(&lines[..4], Path::new("/a"), &Default::default(), &[]);
        assert_eq!((t[1]["text"].as_str(), t[1]["act"]["state"].as_str(), t[1]["act"]["undo"]["possible"].as_bool(), t[1]["act"]["id"].as_str()), (Some("Opened 2 sessions in M5 Pro: fix login and docs."), Some("done"), Some(false), Some("r0123456789ab")));
        let t = thread_with(&lines, Path::new("/a"), &Default::default(), &[]);
        assert_eq!((t[1]["text"].as_str(), t[1]["act"]["state"].as_str(), t[1]["act"]["closing"].as_str(), t[1]["act"]["line"].as_str()), (Some("Closed it. Nothing else changed."), Some("undone"), Some("Closed it. Nothing else changed."), Some("Opened 2 sessions in M5 Pro: fix login and docs.")));
        // Asked in a space's view: its act is that conversation's, and not the main one's.
        let there = act(&[("t9", "fix-login")], Some("sandbox"));
        let lines: Vec<(usize, Entry)> = vec![(1, at(there.entry(), 2))];
        let t = thread_of(&lines, Path::new("/a"), &Default::default(), &[], &[there.view()]);
        assert_eq!(t[0]["space"], "sandbox");
        assert!(in_conversation(&t[0], Some("Sandbox")) && !in_conversation(&t[0], None) && !in_conversation(&t[0], Some("M5 Pro")));
        let main = thread_of(&[(1, at(one.entry(), 2))], Path::new("/a"), &Default::default(), &[], &[]);
        assert!(in_conversation(&main[0], None) && !in_conversation(&main[0], Some("M5 Pro")));
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
        let t = thread_with(&lines, root, &Default::default(), &[]);
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
    fn the_record_is_not_the_chat() {
        let line = |n: usize, x: Entry| (n, x);
        let mut done = e("job", 1, "all tests pass");
        done.status = Some("done".into());
        done.record = true;
        let mut finished = e("alert", 2, "api is done");
        finished.status = Some("done".into());
        finished.record = true;
        let mut blocked = e("alert", 3, "api is waiting for you");
        blocked.status = Some("blocked".into());
        // What the brain wrote on a note turn at a quiet level.
        let mut note = e("note", 4, "The api session finished the login fix.");
        note.record = true;
        let mut peer = e("peer", 5, "which port does the api use?");
        peer.session = Some("t5".into());
        peer.from = Some("t3".into());
        // A receipt the user was told, and one kept for the record.
        let mut told = e("receipt", 6, "The login fix is merged into dev.");
        told.agenda = Some("a3".into());
        let mut kept = e("receipt", 7, "The docs are updated.");
        kept.record = true;
        let mut raised = e("raised", 8, "Merge PR #42 into main?");
        raised.why = Some("merge".into());
        let mut report = e("watch", 9, "t3 (api) is now done. You had asked it: x.");
        report.record = true;
        let lines: Vec<(usize, Entry)> =
            vec![done, finished, blocked, note, peer, told, kept, raised, report].into_iter().enumerate().map(|(i, x)| line(i + 1, x)).collect();
        let t = thread_with(&lines, Path::new("/a"), &Default::default(), &[]);
        let ids: Vec<&str> = t.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["h3", "h6"], "what needs the user, and the receipt they were told");
        assert_eq!((t[1]["role"].as_str(), t[1]["kind"].as_str(), t[1]["text"].as_str()), (Some("tower"), Some("receipt"), Some("The login fix is merged into dev.")));
        // The same entries as the tower wrote them when it told everything: all of them messages.
        let mut loud = e("job", 1, "all tests pass");
        loud.status = Some("done".into());
        assert!(shown(&loud) && shown(&e("watch", 2, "t3 (api) is now done.")) && shown(&e("answer", 3, "It's done.")));
        assert!(!shown(&e("note", 4, "tower log")) && !shown(&e("peer", 5, "hello")), "never, at any level");
    }

    #[test]
    fn a_pr_chip_shows_its_pr_as_it_is_now() {
        let mut chip = e("pr", 5, "release: tickets");
        chip.id = Some("o/app#12".into());
        chip.session = Some("t58".into());
        chip.name = Some("mediaroom".into());
        let lines = vec![(1, e("request", 1, "release it")), (2, chip)];
        let mut prs = std::collections::BTreeMap::new();
        let mut pr = crate::prs::Pr { repo: "o/app".into(), number: 12, base: "main".into(), state: "open".into(), waiting: true, checked_ms: 9, ..Default::default() };
        pr.checks.state = "pending".into();
        prs.insert(pr.key(), pr.clone());
        let t = thread_with(&lines, Path::new("/a"), &prs, &[]);
        assert_eq!((t[1]["role"].as_str(), t[1]["id"].as_str(), t[1]["session"].as_str()), (Some("pr"), Some("h2"), Some("t58")));
        assert_eq!((t[1]["pr"]["key"].as_str(), t[1]["pr"]["waiting"].as_bool(), t[1]["pr"]["checks"]["state"].as_str()), (Some("o/app#12"), Some(true), Some("pending")));
        assert_eq!(t[1]["updated_at"], 9);
        // Merged since: the same message, changed (an `update` for the phones), not a new one.
        pr.state = "merged".into();
        pr.checked_ms = 20;
        prs.insert(pr.key(), pr);
        let t2 = thread_with(&lines, Path::new("/a"), &prs, &[]);
        assert_eq!((t2.len(), t2[1]["id"].as_str(), t2[1]["pr"]["state"].as_str()), (2, Some("h2"), Some("merged")));
        assert_eq!(t2[1]["pr"]["merge"]["can"], false);
    }

    #[test]
    fn reports_show_the_news_not_the_brains_instructions() {
        let merged = "t67 (rel): the user merged PR #42 into main (o/app) from the Mac. I queued it \"The user merged PR #42 (o/app) into main.\", so it carries on from there. Tell the user in one short line only if there's something more for them to do. This is from the space Space B, not the one in focus (Space A), but it's work the user asked for: tell them in one short line that starts with \"In Space B\".";
        assert_eq!(
            report_text(merged),
            "t67 (rel): the user merged PR #42 into main (o/app) from the Mac. I queued it \"The user merged PR #42 (o/app) into main.\", so it carries on from there."
        );
        assert_eq!(report_text("t3 (api) is now done. You had asked it: x. Read its screen and tell me what happened, briefly."), "t3 (api) is now done. You had asked it: x.");
        let quiet = crate::tell::note_at(crate::tell::Level::Decisions, "t3 (api) is now done. You had asked it: x. Read its screen and tell me what happened, briefly.");
        assert_eq!(report_text(&quiet), "t3 (api) is now done. You had asked it: x.");
    }

    #[test]
    fn a_spaces_own_conversation_is_told_apart_from_the_main_one() {
        let at = |mut x: Entry, space: &str| {
            x.space = Some(space.into());
            x
        };
        let mut kept = e("answer", 9, "noted");
        kept.record = true;
        let mut done = e("job", 12, "all tests pass");
        done.status = Some("done".into());
        let lines: Vec<(usize, Entry)> = vec![
            e("request", 1, "what's everyone doing?"),
            // Asked in a space's view, of its own tower: the request and the answer carry the space.
            at(e("request", 2, "what's left here?"), "Sandbox"),
            e("answer", 3, "Nothing is blocked."),
            at(e("answer", 4, "Two tasks are left."), "Sandbox"),
            // The space has no tower of its own: the main one gets it with the space named in front.
            e("request", 5, "In b2c support: any news?"),
            e("request", 6, "and the weather?"),
            e("answer", 7, "No news in B2C support."),
            e("answer", 8, "I don't know the weather."),
            kept,
            // A receipt names the space its request was about, and news its session's.
            at(e("receipt", 10, "The login is fixed."), "Sandbox"),
            e("request", 11, "In the morning: remind me"),
            at(done, "Sandbox"),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, x)| (i + 1, x))
        .collect();
        let spaces = ["Sandbox".to_string(), "B2C support".to_string(), "B2C".to_string()];
        let t = thread_with(&lines, Path::new("/a"), &Default::default(), &spaces);
        let space = |id: &str| t.iter().find(|m| m["id"] == id).map(|m| m["space"].clone()).unwrap();
        assert_eq!((space("h1"), space("h3")), (Value::Null, Value::Null), "asked of the tower, and its answer");
        assert_eq!((space("h2"), space("h4")), (json!("Sandbox"), json!("Sandbox")));
        assert_eq!(space("h5"), json!("B2C support"), "the longest name that fits, as the tower keeps it");
        assert_eq!((space("h7"), space("h8")), (json!("B2C support"), Value::Null), "each answer is its own request's, in order");
        assert_eq!((space("h10"), space("h12")), (json!("Sandbox"), json!("Sandbox")));
        assert_eq!(space("h11"), Value::Null, "\"In the morning\" names no space");
        let ids = |space: Option<&str>| t.iter().filter(|m| in_conversation(m, space)).map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        assert_eq!(ids(Some("sandbox")), ["h2", "h4", "h10"], "its requests, its answers, and the receipt about it");
        assert_eq!(ids(Some("B2C support")), ["h5", "h7"]);
        assert_eq!(ids(None), ["h1", "h3", "h6", "h8", "h10", "h11", "h12"], "the main one keeps the receipt and the news");
        assert!(ids(Some("nowhere")).is_empty());
        // A request nobody answered for half an hour doesn't take the next one's answer.
        let late: Vec<(usize, Entry)> =
            vec![(1, e("request", 1, "In Sandbox: hello?")), (2, e("request", 1 + UNANSWERED_MS, "what's up?")), (3, e("answer", 2 + UNANSWERED_MS, "All quiet."))];
        let t = thread_with(&late, Path::new("/a"), &Default::default(), &spaces);
        assert_eq!((t[0]["space"].clone(), t[2]["space"].clone()), (json!("Sandbox"), Value::Null));
        assert_eq!(asked_in("In Sandbox:", &spaces).as_deref(), Some("Sandbox"));
        assert_eq!(asked_in("in Sandbox: x", &spaces), None, "as the tower writes it: a capital In");
        assert_eq!(asked_in("In Sand: x", &spaces), None);
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
