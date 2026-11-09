//! Telling separate tasks apart in what the user asked: the parts of an
//! instruction, and the projects each part names. Used to warn the brain
//! before it hands a bundle of unrelated tasks to one session (the brain's
//! `brain-guard` hook), and to point out a likely split with the request
//! (`brain::ask`). Deliberately cautious: it only speaks up when two parts
//! name two different projects.

use std::collections::BTreeSet;

/// A project the tower knows: its folder, and the words it goes by.
#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub folder: String,
    /// Normalized names: the folder's name ("herder tower"), and aliases.
    pub names: Vec<String>,
}

impl Project {
    pub fn of_folder(folder: &str) -> Option<Project> {
        let base = folder.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let name = normalize(base);
        // Too generic to mean one project.
        if name.len() < 4 || GENERIC.contains(&name.as_str()) {
            return None;
        }
        Some(Project { folder: folder.into(), names: vec![name] })
    }

    /// The name it's best known by.
    pub fn name(&self) -> &str {
        self.names.first().map(String::as_str).unwrap_or("")
    }
}

const GENERIC: &[&str] =
    &["work", "code", "src", "app", "apps", "web", "home", "projects", "repos", "tmp", "test", "tests", "docs", "main", "server", "client"];

/// Lower case, words only (`herder-tower` and "Herder Tower" are the same).
pub fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// The parts of an instruction that read as tasks of their own: numbered or
/// bulleted lines, else sentences and clauses joined by "also", "and then",
/// "separately", "plus", ";".
pub fn parts(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let listed: Vec<String> = lines.iter().filter(|l| is_list_item(l)).map(|l| strip_marker(l).to_string()).collect();
    if listed.len() >= 2 {
        return listed;
    }
    let mut out: Vec<String> = vec![];
    let mut cur = String::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let lw = normalize(w);
        let next = words.get(i + 1).map(|n| normalize(n)).unwrap_or_default();
        let ends_sentence = w.ends_with('.') || w.ends_with(';') || w.ends_with('?') || w.ends_with('!');
        let joiner = matches!(lw.as_str(), "also" | "separately" | "additionally" | "plus")
            || (lw == "and" && matches!(next.as_str(), "also" | "then" | "separately" | "in"))
            || (lw == "then" && cur.trim().len() > 12);
        if joiner && !cur.trim().is_empty() {
            out.push(cur.trim().to_string());
            cur.clear();
        }
        cur.push_str(w);
        cur.push(' ');
        if ends_sentence {
            out.push(cur.trim().to_string());
            cur.clear();
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.retain(|p| normalize(p).split(' ').count() >= 2);
    out
}

fn is_list_item(l: &str) -> bool {
    let l = l.trim_start();
    if l.starts_with("- ") || l.starts_with("* ") || l.starts_with("• ") {
        return true;
    }
    let digits: String = l.chars().take_while(|c| c.is_ascii_digit()).collect();
    !digits.is_empty() && (l[digits.len()..].starts_with(". ") || l[digits.len()..].starts_with(") "))
}

fn strip_marker(l: &str) -> &str {
    let l = l.trim_start();
    for m in ["- ", "* ", "• "] {
        if let Some(r) = l.strip_prefix(m) {
            return r.trim();
        }
    }
    let digits = l.chars().take_while(|c| c.is_ascii_digit()).count();
    l[digits..].trim_start_matches(['.', ')']).trim()
}

/// The projects a piece of text names (each once).
pub fn named(text: &str, projects: &[Project]) -> Vec<String> {
    let t = format!(" {} ", normalize(text));
    // Each project with the longest of its names the text holds.
    let mut hits: Vec<(String, String)> = vec![];
    for p in projects {
        if let Some(n) = p.names.iter().filter(|n| t.contains(&format!(" {n} "))).max_by_key(|n| n.len())
            && !hits.iter().any(|(f, _)| f == &p.folder)
        {
            hits.push((p.folder.clone(), n.clone()));
        }
    }
    // "herder tower" inside "herder tower android" is the longer one only.
    let longer = |n: &str| hits.iter().any(|(_, m)| m.len() > n.len() && format!(" {m} ").contains(&format!(" {n} ")));
    hits.iter().filter(|(_, n)| !longer(n)).map(|(f, _)| f.clone()).collect()
}

/// Separate tasks for different projects, if the text holds them: each
/// part with the one project it names. None when it reads as one task (or
/// one project).
pub fn separate(text: &str, projects: &[Project]) -> Option<Vec<(String, String)>> {
    let parts = parts(text);
    if parts.len() < 2 {
        return None;
    }
    let mut out: Vec<(String, String)> = vec![];
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for p in &parts {
        let named = named(p, projects);
        if named.len() == 1 {
            seen.insert(named[0].clone());
            out.push((named[0].clone(), p.clone()));
        }
    }
    (seen.len() >= 2).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projects() -> Vec<Project> {
        let mut v: Vec<Project> =
            ["/Users/me/Work/herder-tower", "/Users/me/Work/herder-tower-android", "/Users/me/Work/eyz/eyzflow", "/Users/me/Work/app"]
                .iter()
                .filter_map(|f| Project::of_folder(f))
                .collect();
        v[2].names.push("dashboard".into());
        v
    }

    #[test]
    fn generic_folders_are_no_project() {
        assert!(Project::of_folder("/Users/me/Work/app").is_none());
        assert_eq!(Project::of_folder("/Users/me/Work/herder-tower/").unwrap().name(), "herder tower");
    }

    #[test]
    fn parts_of_a_spoken_request() {
        assert_eq!(parts("Fix the login bug in eyzflow and also add a mute button to herder tower").len(), 2);
        assert_eq!(parts("1. update the docs\n2. bump the version\n3. run the tests").len(), 3);
        assert_eq!(parts("Fix the login bug and add a test for it").len(), 1, "one task, one 'and'");
    }

    #[test]
    fn two_projects_are_two_tasks() {
        let p = projects();
        let s = separate("Fix the login bug in eyzflow. Also add a mute button to herder tower.", &p).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].0, "/Users/me/Work/eyz/eyzflow");
        assert_eq!(s[1].0, "/Users/me/Work/herder-tower");
        // An alias counts.
        assert!(separate("Restart the dashboard dev server; then in herder tower android fix the crash", &p).is_some());
        // Herder tower is not herder tower android.
        assert_eq!(named("the herder tower android app", &p), vec!["/Users/me/Work/herder-tower-android".to_string()]);
    }

    #[test]
    fn one_project_is_one_task() {
        let p = projects();
        assert!(separate("In herder tower fix the queue. Then run the tests in herder tower.", &p).is_none());
        assert!(separate("Fix the queue and run the tests", &p).is_none());
        assert!(separate("Compare how eyzflow and herder tower do auth", &p).is_none(), "one part naming two is one task");
    }
}
