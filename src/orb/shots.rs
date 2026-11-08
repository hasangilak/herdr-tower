//! Screenshots sessions sent to the tower (`tower screenshot`): what the
//! panel's gallery and the viewer show.
//!
//! They come from `history.jsonl` (entries of kind `screenshot`: `id`,
//! `session`, `account`, `caption`, `path`, `at`, maybe `job`) and from the
//! recent list in `state.json` (`screenshots`, newest first, the last 30),
//! which an older daemon doesn't write. Thumbnails are made
//! once per file, in the background, into the tower's data folder.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;

#[derive(Debug, Clone, PartialEq)]
pub struct Shot {
    /// The tower's id for it (the phone API serves it by this).
    pub id: String,
    pub at: u64,
    /// The session's address.
    pub session: String,
    /// Its name, when the entry has one.
    pub name: String,
    pub space: String,
    pub account: Option<String>,
    pub caption: String,
    /// The copy under the tower's attachments.
    pub path: PathBuf,
    pub job: Option<String>,
}

impl Shot {
    fn of(v: &Value) -> Option<Shot> {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let path = s("path");
        if path.is_empty() {
            return None;
        }
        let caption = if v["caption"].is_string() { s("caption") } else { s("text") };
        Some(Shot {
            id: s("id"),
            at: v["at"].as_u64().unwrap_or(0),
            session: s("session"),
            name: s("name"),
            space: s("space"),
            account: v["account"].as_str().filter(|a| !a.is_empty()).map(String::from),
            caption,
            path: PathBuf::from(path),
            job: v["job"].as_str().filter(|j| !j.is_empty()).map(String::from),
        })
    }

    /// A history entry of kind `screenshot`.
    pub fn from_entry(e: &crate::history::Entry) -> Option<Shot> {
        Shot::of(&serde_json::to_value(e).ok()?)
    }

    /// Who sent it: its name, else its address.
    pub fn who(&self) -> &str {
        if self.name.is_empty() { &self.session } else { &self.name }
    }
}

/// The screenshots in history, newest first.
pub fn from_history(text: &str) -> Vec<Shot> {
    let mut out: Vec<Shot> = text
        .lines()
        // Most lines aren't: skip them without parsing.
        .filter(|l| l.contains("\"screenshot\""))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["kind"] == "screenshot")
        .filter_map(|v| Shot::of(&v))
        .collect();
    out.reverse();
    out
}

pub fn load() -> Vec<Shot> {
    from_history(&std::fs::read_to_string(crate::history::path()).unwrap_or_default())
}

/// History's screenshots with the state's recent list (one may lag the
/// other), each once, newest first; names filled in from the radar.
pub fn merge(history: &[Shot], state: &Value) -> Vec<Shot> {
    let mut all: Vec<Shot> = history.to_vec();
    if let Some(list) = state["screenshots"].as_array() {
        for s in list.iter().filter_map(Shot::of) {
            if !all.iter().any(|h| h.path == s.path || (!h.id.is_empty() && h.id == s.id)) {
                all.push(s);
            }
        }
    }
    let agents = state["agents"].as_array().cloned().unwrap_or_default();
    for s in all.iter_mut().filter(|s| s.name.is_empty()) {
        if let Some(a) = agents.iter().find(|a| a["address"] == s.session.as_str()) {
            s.name = a["name"].as_str().unwrap_or("").to_string();
        }
    }
    all.sort_by(|a, b| b.at.cmp(&a.at));
    all
}

/// Where a file's thumbnail goes: named after the file and its size and
/// time, so a changed file gets a new one.
pub fn thumb_path(dir: &Path, src: &Path) -> PathBuf {
    let meta = std::fs::metadata(src).ok();
    let len = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    // FNV-1a: stable across runs, unlike the std hasher.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in src.to_string_lossy().bytes().chain(len.to_le_bytes()).chain(modified.to_le_bytes()) {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    dir.join(format!("{h:016x}.png"))
}

/// The longest side of a thumbnail, in pixels: a chat card shows one about
/// 290 points wide, so twice that on a Retina screen, and readable.
const THUMB: u32 = 800;

/// Makes the thumbnail if it isn't there yet (slow: call it off the main thread).
pub fn make_thumb(dir: &Path, src: &Path) -> anyhow::Result<PathBuf> {
    let out = thumb_path(dir, src);
    if out.exists() {
        return Ok(out);
    }
    let img = image::open(src)?;
    let small = img.thumbnail(THUMB, THUMB);
    std::fs::create_dir_all(dir)?;
    // Written whole, then moved in: a half-written one is never shown.
    let tmp = out.with_extension(format!("tmp{}", std::process::id()));
    small.save_with_format(&tmp, image::ImageFormat::Png)?;
    std::fs::rename(&tmp, &out)?;
    Ok(out)
}

pub fn thumbs_dir() -> PathBuf {
    config::data_dir().join("thumbs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn screenshots_come_from_history_and_state() {
        let history = [
            r#"{"at":1,"kind":"request","text":"take a screenshot"}"#,
            r#"{"at":2,"kind":"screenshot","session":"t3","account":"work","caption":"The login page","path":"/a/1.png","job":"j1"}"#,
            r#"{"at":3,"kind":"note","text":"a \"screenshot\" mentioned"}"#,
            r#"{"at":4,"kind":"screenshot","session":"t4","caption":"","path":"/a/2.png"}"#,
            r#"{"at":5,"kind":"screenshot","session":"t4","caption":"no file"}"#,
        ]
        .join("\n");
        let h = from_history(&history);
        assert_eq!(h.iter().map(|s| s.at).collect::<Vec<_>>(), vec![4, 2]);
        assert_eq!(h[1].caption, "The login page");
        assert_eq!(h[1].account.as_deref(), Some("work"));
        assert_eq!(h[1].job.as_deref(), Some("j1"));
        assert_eq!(h[0].account, None);

        // The state's list adds what history doesn't have yet, once; names come from the radar.
        let state = json!({
            "agents": [{ "address": "t4", "name": "orb-rust" }],
            "screenshots": [
                { "at": 6, "session": "t3", "caption": "Newer", "path": "/a/3.png" },
                { "at": 2, "session": "t3", "caption": "The login page", "path": "/a/1.png" }
            ]
        });
        let all = merge(&h, &state);
        assert_eq!(all.iter().map(|s| s.at).collect::<Vec<_>>(), vec![6, 4, 2]);
        assert_eq!(all[1].who(), "orb-rust");
        assert_eq!(all[0].who(), "t3");
        // No list at all (an older daemon): history alone.
        assert_eq!(merge(&h, &json!({})).len(), 2);
    }

    #[test]
    fn a_thumbnail_is_small_and_made_once() {
        let dir = std::env::temp_dir().join(format!("tower-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("shot.png");
        image::RgbImage::from_fn(1600, 1000, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90])).save(&src).unwrap();
        let t = make_thumb(&dir.join("thumbs"), &src).unwrap();
        let (w, h) = image::image_dimensions(&t).unwrap();
        assert_eq!((w, h), (800, 500));
        let made = std::fs::metadata(&t).unwrap().modified().unwrap();
        assert_eq!(make_thumb(&dir.join("thumbs"), &src).unwrap(), t);
        assert_eq!(std::fs::metadata(&t).unwrap().modified().unwrap(), made);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
