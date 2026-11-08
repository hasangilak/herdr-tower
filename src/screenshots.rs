//! Visual results: `tower screenshot <image>...` puts a session's screenshot
//! where the user looks, the tower's history (the orb's panel, the phone).
//!
//! Each image is checked (PNG, JPEG, WebP or GIF, by its first bytes, at most
//! 15 MB), copied to `~/.local/share/herder-tower/attachments/screenshots/`
//! as `<ms>-<name>` (that name is its id), and recorded as a history entry of
//! kind `screenshot`. Copies older than 30 days are removed; nothing else in
//! the attachments folder (the user's own files) is ever touched.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::history::{self, Entry};

pub const MAX_BYTES: u64 = 15 * 1024 * 1024;
pub const KEEP: Duration = Duration::from_secs(30 * 24 * 3600);
/// How many the state (and the orb) carries.
pub const RECENT: usize = 30;

pub fn dir() -> PathBuf {
    history::attachments_dir().join("screenshots")
}

/// The image type, from its first bytes (not its name).
pub fn kind_of(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

pub fn content_type(id: &str) -> &'static str {
    match id.rsplit('.').next().unwrap_or("") {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// A file name that is safe in a path and a URL, with the right extension.
pub fn stored_name(at_ms: u64, original: &Path, ext: &str) -> String {
    let stem = original.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let mut clean: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    clean = clean.trim_matches('-').chars().take(60).collect();
    if clean.is_empty() {
        clean = "screenshot".into();
    }
    format!("{at_ms}-{clean}.{ext}")
}

/// An id as `GET /v1/screenshots/<id>` takes it: a stored name, nothing more.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 100 && !id.starts_with('.') && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

/// Checks an image and copies it into `into`. Returns the copy's path.
pub fn store(src: &Path, into: &Path, at_ms: u64) -> Result<PathBuf> {
    let meta = std::fs::metadata(src).with_context(|| format!("{}: no such file", src.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", src.display());
    }
    if meta.len() > MAX_BYTES {
        bail!("{} is {:.1} MB; screenshots are {} MB at most", src.display(), meta.len() as f64 / 1e6, MAX_BYTES / 1024 / 1024);
    }
    let mut head = [0u8; 16];
    let n = std::io::Read::read(&mut std::fs::File::open(src)?, &mut head)?;
    let Some(ext) = kind_of(&head[..n]) else { bail!("{} isn't a PNG, JPEG, WebP or GIF image", src.display()) };
    std::fs::create_dir_all(into)?;
    let mut dst = into.join(stored_name(at_ms, src, ext));
    // Two in the same millisecond with the same name.
    let mut k = 1;
    while dst.exists() {
        dst = into.join(stored_name(at_ms, src, ext).replacen('-', &format!("-{k}-"), 1));
        k += 1;
    }
    std::fs::copy(src, &dst).with_context(|| format!("copying {}", src.display()))?;
    Ok(dst)
}

/// Removes stored screenshots older than `keep` (only files in `dir`, only images). How many went.
pub fn prune(dir: &Path, keep: Duration) -> usize {
    let Ok(list) = std::fs::read_dir(dir) else { return 0 };
    let now = SystemTime::now();
    let mut n = 0;
    for e in list.flatten() {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let is_image = p.extension().and_then(|x| x.to_str()).is_some_and(|x| ["png", "jpg", "gif", "webp"].contains(&x));
        let old = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).is_some_and(|age| age > keep);
        if meta.is_file() && is_image && old && std::fs::remove_file(&p).is_ok() {
            n += 1;
        }
    }
    n
}

/// A screenshot entry as the API and the state carry it.
pub fn row(e: &Entry) -> Value {
    json!({
        "id": e.id, "at": e.at, "session": e.session, "name": e.name, "space": e.space, "account": e.account,
        "job": e.job, "caption": e.caption.clone().unwrap_or_default(), "path": e.path,
        "url": e.id.as_ref().map(|id| format!("/v1/screenshots/{id}")),
    })
}

/// The latest screenshots still on disk, newest first.
pub fn recent(entries: &[Entry], n: usize) -> Vec<&Entry> {
    entries.iter().rev().filter(|e| e.kind == "screenshot" && e.path.as_ref().is_some_and(|p| Path::new(p).exists())).take(n).collect()
}

/// `tower screenshot <image>... [--session t] [--caption "..."]`.
pub fn cmd(images: &[PathBuf], session: Option<String>, caption: Option<String>) -> Result<()> {
    if images.is_empty() {
        bail!("tower screenshot <image> [<image>...] [--session <t>] [--caption \"what it shows\"]");
    }
    // The caller's own terminal, when it runs in Herdr.
    let session = session.or_else(|| std::env::var("HC_TERMINAL_ID").ok().filter(|s| !s.is_empty()));
    let caption = caption.map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")).unwrap_or_default();
    // Check them all before keeping any.
    for i in images {
        let meta = std::fs::metadata(i).with_context(|| format!("{}: no such file", i.display()))?;
        if meta.len() > MAX_BYTES {
            bail!("{} is over {} MB", i.display(), MAX_BYTES / 1024 / 1024);
        }
    }
    let base = match &session {
        Some(t) => history::about("screenshot", t, &caption),
        None => Entry { kind: "screenshot".into(), text: caption.clone(), ..Default::default() },
    };
    let job = base.session.as_ref().and_then(|s| history::jobs(&history::load()).into_iter().find(|j| &j.session == s && j.open()).map(|j| j.id));
    let mut kept = vec![];
    for i in images {
        let at = history::now_ms();
        let path = store(i, &dir(), at)?;
        let id = path.file_name().map(|n| n.to_string_lossy().into_owned());
        let e = Entry {
            at,
            id,
            caption: Some(caption.clone()),
            path: Some(path.to_string_lossy().into_owned()),
            job: job.clone(),
            ..base.clone()
        };
        history::append(e);
        kept.push(path);
    }
    // The voice shows them in its state (the orb, the phone).
    let _ = crate::protocol::send(&crate::protocol::Request::Screenshots);
    prune(&dir(), KEEP);
    let who = base.name.clone().or(session.clone()).map(|s| format!(" from {s}")).unwrap_or_default();
    println!(
        "Shown in the tower: {} screenshot{}{who}{}.",
        kept.len(),
        if kept.len() == 1 { "" } else { "s" },
        if caption.is_empty() { String::new() } else { format!(" ({caption})") }
    );
    Ok(())
}

/// `tower screenshots --last N`.
pub fn cmd_list(last: usize, session: Option<&str>) -> Result<()> {
    let entries = history::load();
    let list: Vec<&Entry> = recent(&entries, usize::MAX).into_iter().filter(|e| session.is_none_or(|s| e.session.as_deref() == Some(s))).take(last).collect();
    if std::io::stdout().is_terminal() {
        if list.is_empty() {
            println!("No screenshots yet.");
        }
        for e in &list {
            println!(
                "{:<10} {:<22} {}  {}",
                e.session.as_deref().unwrap_or("-"),
                e.name.as_deref().unwrap_or(""),
                e.caption.as_deref().unwrap_or(""),
                e.path.as_deref().unwrap_or("")
            );
        }
    } else {
        println!("{}", serde_json::to_string_pretty(&json!({ "screenshots": list.iter().map(|e| row(e)).collect::<Vec<_>>() }))?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tower-shots-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn knows_images_by_their_bytes() {
        assert_eq!(kind_of(PNG), Some("png"));
        assert_eq!(kind_of(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(kind_of(b"GIF89a.."), Some("gif"));
        assert_eq!(kind_of(b"RIFF\0\0\0\0WEBPVP8 "), Some("webp"));
        assert_eq!(kind_of(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(kind_of(b"<svg"), None);
        assert_eq!(kind_of(b""), None);
        assert_eq!(content_type("1-a.webp"), "image/webp");
    }

    #[test]
    fn stores_a_checked_copy() {
        let d = tmp("store");
        let src = d.join("Login page (after).PNG");
        std::fs::write(&src, PNG).unwrap();
        let into = d.join("screenshots");
        let a = store(&src, &into, 1700).unwrap();
        assert_eq!(a.file_name().unwrap(), "1700-Login-page--after.png", "the extension comes from the bytes");
        let b = store(&src, &into, 1700).unwrap();
        assert_ne!(a, b, "the same name in the same millisecond gets its own file");
        assert_eq!(std::fs::read(&b).unwrap(), PNG);
        assert!(valid_id(a.file_name().unwrap().to_str().unwrap()));

        let fake = d.join("fake.png");
        std::fs::write(&fake, b"not an image").unwrap();
        assert!(store(&fake, &into, 1).unwrap_err().to_string().contains("isn't a PNG"));
        assert!(store(&d.join("missing.png"), &into, 1).is_err());
        assert!(store(&d, &into, 1).is_err(), "a folder is not an image");
        let big = d.join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        assert!(store(&big, &into, 1).unwrap_err().to_string().contains("at most"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn ids_are_names_not_paths() {
        assert!(valid_id("1794160000000-login.png"));
        for bad in ["", "../x.png", "a/b.png", ".hidden", "x y.png"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn prunes_only_old_screenshots() {
        let d = tmp("prune");
        let old = d.join("1-old.png");
        let new = d.join("2-new.png");
        let other = d.join("notes.txt");
        for p in [&old, &new, &other] {
            std::fs::write(p, PNG).unwrap();
        }
        let long_ago = SystemTime::now() - Duration::from_secs(31 * 24 * 3600);
        for p in [&old, &other] {
            std::fs::File::options().write(true).open(p).unwrap().set_modified(long_ago).unwrap();
        }
        assert_eq!(prune(&d, KEEP), 1);
        assert!(!old.exists() && new.exists(), "only the month-old screenshot goes");
        assert!(other.exists(), "anything that isn't a screenshot image stays");
        assert_eq!(prune(&d.join("nope"), KEEP), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rows_and_recent() {
        let d = tmp("rows");
        let p = d.join("5-a.png");
        std::fs::write(&p, PNG).unwrap();
        let e = |at, path: &Path| Entry {
            at,
            kind: "screenshot".into(),
            session: Some("t3".into()),
            account: Some("work".into()),
            id: path.file_name().map(|n| n.to_string_lossy().into_owned()),
            caption: Some("the new login".into()),
            path: Some(path.to_string_lossy().into_owned()),
            ..Default::default()
        };
        let entries = vec![e(1, &d.join("gone.png")), e(5, &p), Entry { kind: "note".into(), ..Default::default() }];
        let r = recent(&entries, 30);
        assert_eq!(r.len(), 1, "files that are gone (pruned) aren't listed");
        let v = row(r[0]);
        assert_eq!(v["url"], "/v1/screenshots/5-a.png");
        assert_eq!((v["session"].as_str(), v["account"].as_str(), v["caption"].as_str()), (Some("t3"), Some("work"), Some("the new login")));
        let line = serde_json::to_string(r[0]).unwrap();
        for k in ["\"kind\":\"screenshot\"", "\"caption\"", "\"path\"", "\"id\""] {
            assert!(line.contains(k), "{k} in {line}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
