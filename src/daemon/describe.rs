//! The merge card's words for each PR (`crate::describe`), made in the
//! daemon: a summary of each description the user is about to read, and a
//! PR without one has its session asked, once, to write it.
//!
//! Only for the PRs in the user's merge queue (open, its session waiting on
//! the merge, ready: `Pr::ready_for_user`): a draft is summarized when it
//! comes in. One model call at a time, on a thread of its own; one that
//! failed is tried again later, three times at most for the same text.

use std::collections::HashMap;
use std::thread;
use std::time::Duration;

use super::sendq::SendQueue;
use crate::describe;
use crate::prs::{Pr, Prs};

/// How often the queue's PRs are looked at.
const EVERY: Duration = Duration::from_secs(15);
/// A summary that failed: tried again after this long, this often at most.
const RETRY_MS: u64 = 5 * 60_000;
const TRIES: u32 = 3;

/// What to do now: the PRs to summarize, and those whose session is asked for a description.
pub fn due(prs: &[Pr]) -> (Vec<String>, Vec<String>) {
    let queue: Vec<&Pr> = prs.iter().filter(|p| p.open() && p.waiting && p.ready_for_user()).collect();
    (
        queue.iter().filter(|p| describe::needs_summary(p)).map(|p| p.key()).collect(),
        queue.iter().filter(|p| describe::needs_asking(p)).map(|p| p.key()).collect(),
    )
}

pub fn start(prs: Prs, sendq: SendQueue) {
    let spawned = thread::Builder::new().name("describe".into()).spawn(move || {
        // key → (the text it failed on, tries, not before).
        let mut failed: HashMap<String, (u64, u32, u64)> = HashMap::new();
        loop {
            let (summarize, ask) = due(&prs.list());
            for k in ask {
                let Some(pr) = prs.get(&k) else { continue };
                match sendq.add(&pr.session, &describe::ask_text(&pr), &[]) {
                    Ok(_) => {
                        eprintln!("[describe] {k} has no description: asked {} to write one", pr.session);
                        prs.set_asked_body(&k);
                    }
                    Err(e) => eprintln!("[describe] {k}: couldn't ask {} for a description: {e:#}", pr.session),
                }
            }
            for k in summarize {
                let Some(pr) = prs.get(&k) else { continue };
                let of = describe::hash(pr.body.as_deref().unwrap_or(""));
                let now = crate::history::now_ms();
                if let Some((h, tries, until)) = failed.get(&k)
                    && *h == of
                    && (*tries >= TRIES || *until > now)
                {
                    continue;
                }
                match describe::summarize(&pr) {
                    Ok(summary) => {
                        failed.remove(&k);
                        prs.set_described(&k, describe::Described { summary, of });
                    }
                    Err(e) => {
                        let tries = failed.get(&k).filter(|f| f.0 == of).map_or(1, |f| f.1 + 1);
                        eprintln!("[describe] {k}: no summary ({tries} of {TRIES}): {e:#}");
                        failed.insert(k, (of, tries, now + RETRY_MS));
                    }
                }
            }
            thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        eprintln!("[describe] couldn't start: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(n: u64, body: Option<&str>) -> Pr {
        Pr { repo: "o/app".into(), number: n, base: "dev".into(), state: "open".into(), waiting: true, session: "t1".into(), body: body.map(String::from), ..Default::default() }
    }

    #[test]
    fn only_the_queues_prs_are_summarized_or_asked_for() {
        let prs = vec![
            pr(1, Some("Adds a column.")),
            pr(2, Some("")),
            Pr { draft: true, ..pr(3, Some("A draft's.")) },
            Pr { draft: true, ..pr(4, Some("")) },
            Pr { waiting: false, ..pr(5, Some("Nobody waits.")) },
            Pr { state: "merged".into(), ..pr(6, Some("")) },
            pr(7, None),
            Pr { described: Some(describe::Described { summary: "s".into(), of: describe::hash("Done.") }), ..pr(8, Some("Done.")) },
        ];
        assert_eq!(due(&prs), (vec!["o/app#1".to_string()], vec!["o/app#2".to_string()]));
        // Under review: not yet; agreed: in.
        let reviewing = Pr { review: Some(crate::reviews::Brief { status: "reviewing".into(), ..Default::default() }), ..pr(9, Some("")) };
        assert_eq!(due(std::slice::from_ref(&reviewing)), (vec![], vec![]));
        let agreed = Pr { review: Some(crate::reviews::Brief { status: "agreed".into(), ..Default::default() }), ..reviewing };
        assert_eq!(due(&[agreed]).1, ["o/app#9"]);
    }
}
