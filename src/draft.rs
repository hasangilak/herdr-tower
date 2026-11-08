//! Whether the orb's panel holds a draft of yours (typed text, an attached
//! image or file). While it does, a press-to-talk take is dictation into that
//! draft: the daemon publishes the final text as `dictated` instead of
//! sending it, and Enter sends the text and the attachments together.
//!
//! A file beside the socket names the orb's pid, so a panel that crashed
//! with a draft in it doesn't keep the voice from sending.

use std::path::PathBuf;

use crate::config;

fn path() -> PathBuf {
    config::state_dir().join("panel-draft")
}

/// The panel says whether it holds a draft now.
pub fn hold(on: bool) {
    if on {
        let _ = std::fs::create_dir_all(config::state_dir());
        let _ = std::fs::write(path(), std::process::id().to_string());
    } else {
        let _ = std::fs::remove_file(path());
    }
}

/// A live panel holds a draft.
pub fn held() -> bool {
    let Ok(text) = std::fs::read_to_string(path()) else { return false };
    match text.trim().parse::<i32>() {
        // Signal 0 only asks whether the process is there.
        Ok(pid) if pid > 0 => unsafe { libc::kill(pid, 0) == 0 },
        _ => false,
    }
}
