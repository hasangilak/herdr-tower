//! Turning an agent's markdown into something worth hearing.

/// Plain spoken text: code blocks, tables, links and markdown marks removed,
/// cut at a sentence end once it passes `max` characters.
pub fn speakable(markdown: &str, max: usize) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code || trimmed.starts_with('|') || trimmed.chars().all(|c| "-=*_ ".contains(c)) {
            continue;
        }
        let line = trimmed.trim_start_matches(['#', '>']).trim_start();
        let line = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")).unwrap_or(line);
        let line = strip_inline(line);
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            // A line break is a pause: end the sentence if it didn't.
            if !out.ends_with(['.', '!', '?', ':', ';', ',']) {
                out.push('.');
            }
            out.push(' ');
        }
        out.push_str(&line);
    }
    truncate_at_sentence(&out, max)
}

fn strip_inline(line: &str) -> String {
    let mut s = String::with_capacity(line.len());
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            // [text](url) -> text
            '[' => {
                if let Some(close) = chars[i..].iter().position(|&c| c == ']').map(|p| p + i)
                    && chars.get(close + 1) == Some(&'(')
                    && let Some(end) = chars[close..].iter().position(|&c| c == ')').map(|p| p + close)
                {
                    s.extend(&chars[i + 1..close]);
                    i = end + 1;
                    continue;
                }
                s.push(c);
            }
            '*' | '`' | '~' => {}
            '_' if i + 1 < chars.len() && chars[i + 1] == '_' => i += 1,
            _ => s.push(c),
        }
        i += 1;
    }
    // Bare URLs are noise out loud.
    s.split_whitespace()
        .map(|w| if w.starts_with("http://") || w.starts_with("https://") { "the link" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_at_sentence(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let cut: String = text.chars().take(max).collect();
    match cut.rfind(['.', '!', '?']) {
        Some(end) if end > max / 3 => format!("{} The rest is on screen.", &cut[..=end]),
        _ => format!("{}... The rest is on screen.", cut.trim_end()),
    }
}

/// Splits text into sentences, joining very short ones so Kokoro gets enough
/// context for natural intonation.
pub fn sentences(text: &str) -> Vec<String> {
    let mut raw = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        cur.push(c);
        let next = chars.get(i + 1).copied();
        let ends = matches!(c, '.' | '!' | '?' | ';' | '\n') && next.is_none_or(char::is_whitespace);
        // Keep "e.g." and "3.5" together: the next char must be whitespace (checked above).
        if ends {
            let s = cur.trim().to_string();
            if !s.is_empty() {
                raw.push(s);
            }
            cur.clear();
        }
    }
    if !cur.trim().is_empty() {
        raw.push(cur.trim().to_string());
    }
    let mut out: Vec<String> = Vec::new();
    for s in raw {
        match out.last_mut() {
            Some(last) if last.chars().count() < 24 => {
                last.push(' ');
                last.push_str(&s);
            }
            _ => out.push(s),
        }
    }
    // Very long sentences are split at commas so the first sound comes fast.
    out.into_iter().flat_map(split_long).collect()
}

fn split_long(s: String) -> Vec<String> {
    if s.chars().count() <= 220 {
        return vec![s];
    }
    let mut parts = Vec::new();
    let mut cur = String::new();
    for piece in s.split_inclusive([',', ';', ':']) {
        if cur.chars().count() + piece.chars().count() > 200 && !cur.is_empty() {
            parts.push(cur.trim().to_string());
            cur.clear();
        }
        cur.push_str(piece);
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim().to_string());
    }
    parts
}

/// Lowercase words only, for matching wake and stop words.
pub fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_alphanumeric() || c == '\'' { c.to_ascii_lowercase() } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What Whisper says when it hears nothing.
pub fn is_hallucination(text: &str) -> bool {
    let n = normalize(text);
    if n.is_empty() {
        return true;
    }
    const NOISE: &[&str] = &[
        "thank you",
        "thanks for watching",
        "thank you for watching",
        "you",
        "bye",
        "blank audio",
        "music",
        "silence",
        "applause",
        "laughter",
        "inaudible",
        "subtitles by the amara org community",
        "please subscribe",
        "so",
        "uh",
        "um",
        "hmm",
    ];
    NOISE.contains(&n.as_str())
        || text.trim().starts_with('[') && text.trim().ends_with(']')
        || text.trim().starts_with('(') && text.trim().ends_with(')')
}

/// Short acknowledgements, English and German, lowercase. Not "yes", "no",
/// "ja", "right": those can be the answer itself.
const ACKS: &[&str] = &[
    "okay sure", "ok sure", "okay", "okey", "ok", "sure thing", "sure", "got it", "gotcha", "i'm on it", "i\u{2019}m on it", "im on it", "on it", "will do",
    "alright", "all right", "understood", "roger that", "roger", "no problem", "absolutely", "certainly", "of course", "sounds good",
    "i'll do that", "i\u{2019}ll do that", "i will do that", "doing it now", "one moment", "one sec", "just a moment", "alles klar", "klar", "mach ich",
    "mache ich", "wird gemacht", "verstanden", "geht klar", "in ordnung", "gerne", "gern", "jawohl", "natürlich", "sofort",
    "ich bin dran", "bin dran", "ich mach das", "ich mache das", "gut",
];

/// The ack phrase `s` starts with (as a whole word, the longest), and the rest after it.
fn leading_ack(s: &str) -> Option<&str> {
    let lower = s.to_lowercase();
    let n = ACKS
        .iter()
        .filter(|a| {
            lower.starts_with(*a) && lower[a.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '\'' && c != '\u{2019}')
        })
        .map(|a| a.len())
        .max()?;
    // The letters in ACKS lowercase from same-length capitals, so `n` holds in `s` too.
    s.get(n..)
}

fn is_mark(c: char) -> bool {
    matches!(c, ',' | '.' | '!' | '?' | ';' | ':' | '-' | '—' | '–' | '…')
}

/// A reply heard right after the tower's own "Okay." / "Got it.": None
/// when it is nothing but an acknowledgement, else the reply without the
/// acknowledgement it starts with ("Sure, the tests pass." -> "The tests pass.").
/// A word after it without a mark ("Sure enough ...", "Klar ist ...") is content.
pub fn without_ack(text: &str) -> Option<String> {
    let mut s = text.trim();
    let mut stripped = false;
    loop {
        let Some(rest) = leading_ack(s) else { break };
        let t = rest.trim_start();
        if t.is_empty() || t.chars().all(|c| is_mark(c) || c.is_whitespace()) {
            return None;
        }
        if t.starts_with(is_mark) {
            s = t.trim_start_matches(|c: char| is_mark(c) || c.is_whitespace());
            stripped = true;
        } else if leading_ack(t).is_some() {
            // "Okay sure, ..."
            s = t;
            stripped = true;
        } else {
            break;
        }
    }
    if !stripped {
        return Some(text.to_string());
    }
    let mut c = s.chars();
    Some(match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => return None,
    })
}

/// The user takes back what they said ("never mind", "forget it", "ignore
/// that", "vergiss es"): the canned acknowledgement is all the answer needed.
pub fn is_dismissal(request: &str) -> bool {
    let n: String = request.to_lowercase().chars().map(|c| if c.is_alphanumeric() || c == '\'' { c } else { ' ' }).collect();
    let n = n.split_whitespace().collect::<Vec<_>>().join(" ");
    let n = n.strip_prefix("tower ").unwrap_or(&n);
    if n.split(' ').count() > 8 {
        return false;
    }
    const START: &[&str] = &[
        "never mind", "nevermind", "forget it", "forget that", "forget about it", "forget what i said", "forget my", "ignore that",
        "ignore this", "ignore it", "ignore my", "ignore what i said", "ignore the last", "scratch that", "cancel that", "disregard",
        "vergiss es", "vergiss das", "ignorier das", "ignoriere das", "schon gut", "lass gut sein",
    ];
    // Only as the whole message: "Nothing is blocked?" is a question.
    const WHOLE: &[&str] = &["nothing", "no nothing", "oh nothing", "sorry nothing", "egal", "nichts", "doch nicht", "nicht so wichtig"];
    WHOLE.contains(&n) || START.iter().any(|p| n == *p || n.starts_with(&format!("{p} ")))
}

/// A short reply: what the brain says to a dismissal is dropped when this short.
pub fn is_short_reply(text: &str) -> bool {
    text.split_whitespace().count() <= 12
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_markdown() {
        let md = "## Done\n\nI sent **eyzflow** the task. See [the PR](https://x.y/1).\n\n```sh\nhc ls\n```\n| a | b |\n- one\n- two";
        assert_eq!(speakable(md, 500), "Done. I sent eyzflow the task. See the PR. one. two");
    }

    #[test]
    fn splits_sentences() {
        let s = sentences("Hi. The tests pass in eyzflow, version 3.5 works. Next?");
        assert_eq!(s, vec!["Hi. The tests pass in eyzflow, version 3.5 works.", "Next?"]);
    }

    #[test]
    fn truncates() {
        let t = speakable(&"This is a sentence. ".repeat(40), 100);
        assert!(t.ends_with("The rest is on screen."));
        assert!(t.len() < 140);
    }

    #[test]
    fn hallucinations() {
        assert!(is_hallucination(" Thank you."));
        assert!(is_hallucination("[BLANK_AUDIO]"));
        assert!(!is_hallucination("Tell eyzflow to run the tests"));
    }

    #[test]
    fn drops_acks_after_the_canned_one() {
        for only in ["Okay, sure.", "okay sure", "Got it!", "On it.", "Will do.", "Sure thing.", "OK.", "Alright — on it.", "I'm on it.", "Alles klar.", "Mach ich!", "Klar, wird gemacht.", "Okay, sure. Got it.", "Verstanden"] {
            assert_eq!(without_ack(only), None, "{only:?}");
        }
        assert_eq!(without_ack("Okay, sure. The tests pass.").as_deref(), Some("The tests pass."));
        assert_eq!(without_ack("Got it, starting the eyzflow session.").as_deref(), Some("Starting the eyzflow session."));
        assert_eq!(without_ack("Alles klar, t22 läuft noch.").as_deref(), Some("T22 läuft noch."));
        assert_eq!(without_ack("okay sure, checking").as_deref(), Some("Checking"));
        // Real content stays as it is.
        for keep in ["Sure enough, the build failed.", "Okayama is a city.", "Yes.", "No, it's still working.", "Right now nothing is blocked.", "Gut gemacht, alles grün.", "The okay button is broken.", "Klarna is down."] {
            assert_eq!(without_ack(keep).as_deref(), Some(keep), "{keep:?}");
        }
    }

    #[test]
    fn dismissals() {
        for d in ["Never mind.", "never mind, it's fine", "Forget it!", "Ignore my last prompt.", "ignore that", "Tower, forget that.", "Scratch that.", "Vergiss es.", "Egal.", "Schon gut.", "Ignorier das bitte", "Oh, nothing."] {
            assert!(is_dismissal(d), "{d:?}");
        }
        for r in [
            "Never mind the tests, deploy the dashboard to production now please",
            "Ignore the failing lint and open the PR into dev for the release",
            "What's everyone doing?",
            "Nothing is blocked?",
            "Egal welche Session, starte die Tests",
        ] {
            assert!(!is_dismissal(r), "{r:?}");
        }
        assert!(is_short_reply("No problem, I'll ignore that."));
        assert!(!is_short_reply("No problem. By the way, the eyzflow session finished and its pull request is ready for your review now."));
    }
}
