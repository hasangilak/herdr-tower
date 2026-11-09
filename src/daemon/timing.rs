//! How long each step of a request takes, written to the daemon's log:
//! `[time] 11:20:03.412 +  612 ms transcribed`, counted from the end of the
//! take (or from `tower ask`). Each step is logged once per request.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Clock {
    start: Instant,
    marked: Vec<&'static str>,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);

/// A request begins: the take ended, or text was asked.
pub fn start(what: &str) {
    *CLOCK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Clock { start: Instant::now(), marked: vec![] });
    eprintln!("[time] {} +    0 ms {what}", wall());
}

/// A step of the current request happened.
pub fn mark(step: &'static str) {
    let mut clock = CLOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = clock.as_mut() else { return };
    // A step this late belongs to something else (an alert, a watch).
    if c.marked.contains(&step) || c.start.elapsed() > Duration::from_secs(300) {
        return;
    }
    c.marked.push(step);
    eprintln!("[time] {} +{:>5} ms {step}", wall(), c.start.elapsed().as_millis());
}

/// Local wall-clock time, to line the log up with the brain's transcript.
fn wall() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut tm) };
    format!("{:02}:{:02}:{:02}.{:03}", tm.tm_hour, tm.tm_min, tm.tm_sec, now.subsec_millis())
}
