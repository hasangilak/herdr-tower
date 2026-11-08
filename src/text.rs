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
}
