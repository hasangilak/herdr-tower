//! Running `tower` from the windows (the program `config::tower_bin` names),
//! for what the full window sets: how much the tower tells, a space's
//! switches, goal and closing, the launcher, a message to a space's tower.
//!
//! - Everything here blocks: call it from the background executor.
//! - A command that fails, or that this tower doesn't have yet, gives its
//!   own error as one line, to show where the user asked (`error_line`).
//! - `ask` with a space runs `tower ask --space <space> …` (the daemon
//!   hands it to that space's own tower, or to the main one with the space
//!   named) and, where that flag isn't known (an older tower), sends the
//!   same words to the main tower with the space named in front.

use std::process::{Command, Stdio};

use crate::config;

/// `tower <args>`: what it printed, or the line that says why not.
pub(super) fn tower<S: AsRef<str>>(args: &[S]) -> Result<String, String> {
    let bin = config::tower_bin();
    let sub = args.first().map(|a| a.as_ref()).unwrap_or("");
    match Command::new(&bin).args(args.iter().map(|a| a.as_ref())).stdin(Stdio::null()).output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        Ok(o) => Err(error_line(sub, &String::from_utf8_lossy(&o.stderr), &String::from_utf8_lossy(&o.stdout))),
        Err(e) => Err(format!("{}: {e}", bin.display())),
    }
}

/// A failed command's error as one line: the first line it wrote (to
/// stderr, else stdout) without `tower:` or `error:` in front, named for the
/// command (`tower tell: …`) unless it names itself.
pub(super) fn error_line(sub: &str, stderr: &str, stdout: &str) -> String {
    let first = |text: &str| text.lines().map(str::trim).find(|l| !l.is_empty()).map(String::from);
    let line = first(stderr).or_else(|| first(stdout)).unwrap_or_default();
    let line = line.strip_prefix("tower: ").unwrap_or(&line);
    let line = line.strip_prefix("error: ").unwrap_or(line).trim();
    match (line.is_empty(), line.starts_with("tower ")) {
        (true, _) => format!("tower {sub} didn't work."),
        (false, true) => line.to_string(),
        (false, false) => format!("tower {sub}: {line}"),
    }
}

/// The error says the command doesn't know `flag` (an older tower).
pub(super) fn refuses(error: &str, flag: &str) -> bool {
    error.contains(flag) && ["unexpected", "unrecognized", "unknown", "invalid"].iter().any(|w| error.contains(w))
}

/// The words for the main tower when a space's own can't be reached by
/// flag: as the daemon itself names a space whose tower can't take a
/// request (one wording, so a space's view finds its reply either way).
pub(super) fn in_space(space: &str, message: &str) -> String {
    crate::daemon::brains::in_space(space, message)
}

/// Hands `message` to the brain as a typed request (`tower ask`); with a
/// space, to that space's tower.
pub(super) fn ask(space: Option<&str>, message: &str) -> Result<String, String> {
    let Some(space) = space else { return tower(&["ask", message]) };
    match tower(&["ask", "--space", space, message]) {
        Err(e) if refuses(&e, "--space") => tower(&["ask", &in_space(space, message)]),
        r => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_command_says_why_in_one_line() {
        // A subcommand this tower doesn't have yet, as clap says it.
        let clap = "error: unrecognized subcommand 'tell'\n\n  tip: a similar subcommand exists: 'talk'\n\nUsage: tower [OPTIONS] [COMMAND]\n";
        assert_eq!(error_line("tell", clap, ""), "tower tell: unrecognized subcommand 'tell'");
        // The tower's own errors name the command already.
        let own = "tower: tower space mute: use `tower space agenda <space> \"<text>\"`\n";
        assert_eq!(error_line("space", own, ""), "tower space mute: use `tower space agenda <space> \"<text>\"`");
        assert_eq!(error_line("launch", "tower: no setup called \"x\"\n", ""), "tower launch: no setup called \"x\"");
        // Nothing on stderr: what it printed; nothing at all: said plainly.
        assert_eq!(error_line("setups", "\n", "no setups yet\n"), "tower setups: no setups yet");
        assert_eq!(error_line("tell", "", ""), "tower tell didn't work.");
    }

    #[test]
    fn a_space_the_flag_cant_reach_is_named_in_the_words() {
        assert!(refuses("tower ask: unexpected argument '--space' found", "--space"));
        assert!(refuses("tower ask: unknown option --space", "--space"));
        // Any other failure is the user's to see, not a reason to send it elsewhere.
        assert!(!refuses("tower ask: the tower isn't running (tower up)", "--space"));
        assert!(!refuses("tower ask: no space called \"--space\"", "--space"));
        assert_eq!(in_space("sandbox", "what's left?"), "In sandbox: what's left?");
    }
}
