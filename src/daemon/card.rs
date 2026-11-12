//! A question set's state (its card), and every move it can make, in one
//! table (`SetState::next`; see docs/state-machines.md, section 2).
//!
//! A set is in two parts that move together: its `phase` (shown, keys going
//! in, answered, ...) and its `decision` (who answers it: nobody yet, the
//! tower, the user). The decision outlives the phase it was taken in: a card
//! the user decides stays theirs while it is answered, failed or tried again.
//!
//! On the wire nothing changes: `status` is the phase's word (`open`,
//! `answering`, `answered`, `queued`, `failed`) and `decision` is `None`,
//! `auto` or `escalate`, as the panel and the phones read them. `sync` keeps
//! those strings in step with the state; nothing else writes them.

use crate::questions::QuestionSet;

/// Who is typing the answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answerer {
    /// The panel, a phone, the CLI: the user's own answer.
    User,
    /// The tower, with its recommendation (the brain said `auto`).
    Tower,
}

/// Who answers the set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Decision {
    /// The brain hasn't said yet (it has 40 s).
    #[default]
    Deciding,
    /// The tower answers it itself.
    Auto,
    /// The user does (the brain, the rules, the policy or a timeout said so).
    Escalated,
}

impl Decision {
    pub fn wire(self) -> Option<&'static str> {
        match self {
            Decision::Deciding => None,
            Decision::Auto => Some("auto"),
            Decision::Escalated => Some("escalate"),
        }
    }

    /// From the wire (or a file an older tower wrote): anything else is undecided.
    pub fn from_wire(s: Option<&str>) -> Decision {
        match s {
            Some("auto") => Decision::Auto,
            Some("escalate") => Decision::Escalated,
            _ => Decision::Deciding,
        }
    }
}

/// Why a set is over: what a late action on it is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloseWhy {
    /// The tower typed the answer, and the dialog went.
    AnsweredHere,
    /// An answer found no dialog any more: answered (or closed) in the terminal.
    AnsweredElsewhere,
    /// The same dialog, read again under a new id.
    Replaced { by: String },
    /// The session ended (or its address is another terminal's now).
    SessionGone,
    /// The session stopped showing it without the tower answering
    /// (answered in the terminal, or it moved on).
    Unblocked,
    /// The session still waits, on a screen the tower can't read now: not
    /// answered, only out of sight.
    Unread,
    /// The brain read the message and found it asks nothing.
    NothingAsked,
    /// The user dismissed a text question.
    Dismissed,
    /// What it asked about was settled elsewhere (`what`: "PR #786 was merged on GitHub").
    Settled { what: String },
}

impl CloseWhy {
    /// A late action on a set closed this way: the error it gets. Starts with
    /// "already answered" when the question was answered, as the phone API
    /// maps it (`answered`).
    pub fn late(&self, id: &str, who: &str) -> String {
        match self {
            CloseWhy::AnsweredHere => format!("already answered: {id} was answered from the tower"),
            CloseWhy::AnsweredElsewhere | CloseWhy::Unblocked => {
                format!("already answered: {who} isn't asking this any more (it was answered or closed in the session)")
            }
            CloseWhy::Replaced { by } => {
                format!("the questions changed since they were shown (now {by}); the card shows them now: check it and submit again")
            }
            CloseWhy::SessionGone => format!("{who} isn't there any more: its question went with it"),
            CloseWhy::Unread => format!("{who} shows something the tower can't read now: answer it in the session"),
            CloseWhy::NothingAsked => format!("{id} turned out to ask nothing"),
            CloseWhy::Dismissed => format!("{id} was dismissed"),
            CloseWhy::Settled { what } => format!("already answered: {what}"),
        }
    }
}

/// Where a set is in its life.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Phase {
    /// Shown (the decision says who answers it).
    #[default]
    Open,
    /// Keys going in, one answer at a time.
    Answering { by: Answerer },
    /// Submitted (or a text reply typed); the dialog hasn't gone yet.
    Answered,
    /// A text question's reply, waiting for the session to stop working.
    Queued,
    /// Typing stopped part-way; it may be tried again.
    Failed { error: String },
    /// The user hid it while the dialog is still up.
    Dismissed,
    /// Over: the dialog went away (or the set was replaced).
    Closed { why: CloseWhy },
}

impl Phase {
    /// The `status` word on the wire.
    pub fn wire(&self) -> &'static str {
        match self {
            Phase::Open | Phase::Dismissed => "open",
            Phase::Answering { .. } => "answering",
            Phase::Answered => "answered",
            Phase::Queued => "queued",
            Phase::Failed { .. } => "failed",
            Phase::Closed { .. } => "closed",
        }
    }
}

/// What happens to a set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ev {
    /// The brain and the rules decided (`questions::decide`).
    Decide { auto: bool },
    /// It goes to the user after all (the screen needs them, typing failed, no word in time).
    Escalate,
    /// Answers start going in.
    Answer { by: Answerer },
    /// They went in, and were submitted (or the dialog has no submit step).
    Submitted,
    /// They went in without submitting: the dialog is still up, still open.
    Filled,
    /// Typing stopped part-way.
    Stopped { error: String },
    /// A text question's reply: typed now, or waiting for the session to stop.
    Reply { queued: bool },
    /// The user hides a dialog that is still up.
    Dismiss,
    /// It is over.
    Close(CloseWhy),
}

/// A move the table doesn't allow: the caller says what it means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Answers are going in already.
    AlreadyGoingIn,
    /// It was answered (or its reply waits to go in).
    AnsweredAlready,
    /// It is over.
    Closed(CloseWhy),
    /// The user hid it.
    Dismissed,
    /// The tower answers only what was decided `auto`.
    NotTheTowers,
    /// It was decided already.
    Decided,
    /// Typing results for a set that isn't being typed into.
    NotAnswering,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Refused::AlreadyGoingIn => "answers are already going in",
            Refused::AnsweredAlready => "it is answered already",
            Refused::Closed(_) => "it is closed",
            Refused::Dismissed => "it was dismissed",
            Refused::NotTheTowers => "the tower answers only what was decided auto",
            Refused::Decided => "it is decided already",
            Refused::NotAnswering => "no answer is going in",
        })
    }
}

impl std::error::Error for Refused {}

/// A set's state: its phase and its decision.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetState {
    pub phase: Phase,
    pub decision: Decision,
}

impl SetState {
    /// The whole table: where `ev` takes this state, or why it can't.
    pub fn next(&self, ev: &Ev) -> Result<SetState, Refused> {
        use Phase::*;
        let d = self.decision;
        let to = |phase: Phase| Ok(SetState { phase, decision: d });
        if let Closed { why } = &self.phase {
            return Err(Refused::Closed(why.clone()));
        }
        match (ev, &self.phase) {
            // The same dialog read again waits for the typing to end.
            (Ev::Close(CloseWhy::Replaced { .. }), Answering { .. }) => Err(Refused::AlreadyGoingIn),
            (Ev::Close(why), _) => to(Closed { why: why.clone() }),
            // A hidden card only closes.
            (_, Dismissed) => Err(Refused::Dismissed),

            (Ev::Decide { .. }, _) if d != Decision::Deciding => Err(Refused::Decided),
            (Ev::Decide { auto }, phase) => {
                Ok(SetState { phase: phase.clone(), decision: if *auto { Decision::Auto } else { Decision::Escalated } })
            }

            (Ev::Escalate, Open | Failed { .. } | Answering { by: Answerer::Tower }) => {
                Ok(SetState { phase: Open, decision: Decision::Escalated })
            }
            (Ev::Escalate, Answering { by: Answerer::User }) => Err(Refused::AlreadyGoingIn),
            (Ev::Escalate, Answered | Queued) => Err(Refused::AnsweredAlready),

            (Ev::Answer { .. } | Ev::Reply { .. } | Ev::Dismiss, Answering { .. }) => Err(Refused::AlreadyGoingIn),
            (Ev::Answer { .. } | Ev::Reply { .. }, Answered | Queued) => Err(Refused::AnsweredAlready),
            (Ev::Answer { by: Answerer::Tower }, _) if d != Decision::Auto => Err(Refused::NotTheTowers),
            (Ev::Answer { by }, Open | Failed { .. }) => to(Answering { by: *by }),
            (Ev::Reply { queued }, Open | Failed { .. }) => to(if *queued { Queued } else { Answered }),

            (Ev::Submitted, Answering { .. }) => to(Answered),
            (Ev::Filled, Answering { .. }) => to(Open),
            (Ev::Stopped { error }, Answering { .. }) => to(Failed { error: error.clone() }),
            (Ev::Submitted | Ev::Filled | Ev::Stopped { .. }, _) => Err(Refused::NotAnswering),

            (Ev::Dismiss, Open | Failed { .. } | Answered | Queued) => to(Dismissed),

            // Returned above already; the compiler can't tell.
            (_, Closed { why }) => Err(Refused::Closed(why.clone())),
        }
    }

    /// The state an older tower (or the wire) describes with its strings.
    /// A set that was being answered when the tower stopped isn't any more:
    /// it failed part-way, for the user to look at and try again.
    pub fn from_wire(status: &str, decision: Option<&str>, error: Option<&str>) -> SetState {
        let phase = match status {
            "answering" => Phase::Failed { error: "the tower stopped while answering it".into() },
            "answered" => Phase::Answered,
            "queued" => Phase::Queued,
            "failed" => Phase::Failed { error: error.unwrap_or("It stopped.").to_string() },
            // "open", "" and anything unknown: shown, as it is.
            _ => Phase::Open,
        };
        SetState { phase, decision: Decision::from_wire(decision) }
    }

    /// Answers are going in (or went in) from the tower: its dialog going away is "answered here".
    pub fn answered_here(&self) -> bool {
        matches!(self.phase, Phase::Answering { .. } | Phase::Answered | Phase::Queued)
    }
}

/// Writes the wire strings (`status`, `decision`, `error`) from `set.state`.
pub fn sync(set: &mut QuestionSet) {
    set.status = set.state.phase.wire().to_string();
    set.decision = set.state.decision.wire().map(String::from);
    set.error = match &set.state.phase {
        Phase::Failed { error } => Some(error.clone()),
        _ => None,
    };
}

/// Moves `set` by `ev`, wire strings and all.
pub fn apply(set: &mut QuestionSet, ev: &Ev) -> Result<(), Refused> {
    set.state = set.state.next(ev)?;
    sync(set);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use Answerer::*;
    use Decision::*;
    use Phase::*;

    fn st(phase: Phase, decision: Decision) -> SetState {
        SetState { phase, decision }
    }
    fn failed() -> Phase {
        Failed { error: "x".into() }
    }
    fn closed(why: CloseWhy) -> Phase {
        Closed { why }
    }

    /// Every row of the design's table, and the refused moves.
    #[test]
    fn the_table() {
        let ok = |from: SetState, ev: Ev, to: SetState| assert_eq!(from.next(&ev), Ok(to), "{from:?} --{ev:?}");
        let no = |from: SetState, ev: Ev, why: Refused| assert_eq!(from.next(&ev), Err(why), "{from:?} --{ev:?}");

        // Deciding -> Auto / Escalated (the brain, the rules).
        ok(st(Open, Deciding), Ev::Decide { auto: true }, st(Open, Auto));
        ok(st(Open, Deciding), Ev::Decide { auto: false }, st(Open, Escalated));
        no(st(Open, Auto), Ev::Decide { auto: false }, Refused::Decided);
        // Decided after the user answered already: the answer stands.
        ok(st(Answered, Deciding), Ev::Decide { auto: true }, st(Answered, Auto));

        // Deciding -> Escalated (no word in time); Auto -> Escalated (the screen needs the user).
        ok(st(Open, Deciding), Ev::Escalate, st(Open, Escalated));
        ok(st(Open, Auto), Ev::Escalate, st(Open, Escalated));
        ok(st(Answering { by: Tower }, Auto), Ev::Escalate, st(Open, Escalated));
        ok(st(failed(), Auto), Ev::Escalate, st(Open, Escalated));
        no(st(Answering { by: User }, Escalated), Ev::Escalate, Refused::AlreadyGoingIn);
        no(st(Answered, Auto), Ev::Escalate, Refused::AnsweredAlready);

        // Auto -> Answering{Tower}; only what was decided auto.
        ok(st(Open, Auto), Ev::Answer { by: Tower }, st(Answering { by: Tower }, Auto));
        no(st(Open, Escalated), Ev::Answer { by: Tower }, Refused::NotTheTowers);
        no(st(Open, Deciding), Ev::Answer { by: Tower }, Refused::NotTheTowers);

        // Escalated / Failed / Deciding -> Answering{User}.
        ok(st(Open, Escalated), Ev::Answer { by: User }, st(Answering { by: User }, Escalated));
        ok(st(failed(), Escalated), Ev::Answer { by: User }, st(Answering { by: User }, Escalated));
        ok(st(Open, Deciding), Ev::Answer { by: User }, st(Answering { by: User }, Deciding));

        // One answer at a time; an answered set isn't answered twice.
        no(st(Answering { by: User }, Escalated), Ev::Answer { by: User }, Refused::AlreadyGoingIn);
        no(st(Answering { by: Tower }, Auto), Ev::Answer { by: User }, Refused::AlreadyGoingIn);
        no(st(Answered, Escalated), Ev::Answer { by: User }, Refused::AnsweredAlready);
        no(st(Queued, Escalated), Ev::Reply { queued: false }, Refused::AnsweredAlready);

        // Answering -> Answered / Open (filled, not submitted) / Failed.
        ok(st(Answering { by: User }, Escalated), Ev::Submitted, st(Answered, Escalated));
        ok(st(Answering { by: User }, Escalated), Ev::Filled, st(Open, Escalated));
        ok(st(Answering { by: User }, Escalated), Ev::Stopped { error: "x".into() }, st(failed(), Escalated));
        no(st(Open, Escalated), Ev::Submitted, Refused::NotAnswering);
        no(st(Answered, Escalated), Ev::Stopped { error: "late".into() }, Refused::NotAnswering);

        // A text question: replied now (Answered) or when the session stops (Queued).
        ok(st(Open, Escalated), Ev::Reply { queued: false }, st(Answered, Escalated));
        ok(st(Open, Escalated), Ev::Reply { queued: true }, st(Queued, Escalated));

        // Dismissed: hidden while it is up; only closing moves it on.
        ok(st(Open, Escalated), Ev::Dismiss, st(Dismissed, Escalated));
        no(st(Answering { by: User }, Escalated), Ev::Dismiss, Refused::AlreadyGoingIn);
        no(st(Dismissed, Escalated), Ev::Answer { by: User }, Refused::Dismissed);
        no(st(Dismissed, Escalated), Ev::Escalate, Refused::Dismissed);
        ok(st(Dismissed, Escalated), Ev::Close(CloseWhy::Unblocked), st(closed(CloseWhy::Unblocked), Escalated));

        // Replaced: from anything but Answering (the re-read waits for the typing).
        let by = || CloseWhy::Replaced { by: "q2".into() };
        ok(st(Open, Escalated), Ev::Close(by()), st(closed(by()), Escalated));
        ok(st(Answered, Escalated), Ev::Close(by()), st(closed(by()), Escalated));
        no(st(Answering { by: User }, Escalated), Ev::Close(by()), Refused::AlreadyGoingIn);

        // The outside world: answered in the terminal, unblocked, gone. From anything.
        for from in [Open, Answering { by: User }, Answered, Queued, failed()] {
            for why in [CloseWhy::AnsweredElsewhere, CloseWhy::Unblocked, CloseWhy::SessionGone, CloseWhy::AnsweredHere] {
                ok(st(from.clone(), Escalated), Ev::Close(why.clone()), st(closed(why), Escalated));
            }
        }

        // Closed is the end: every late move says how it ended.
        let over = st(closed(CloseWhy::AnsweredElsewhere), Escalated);
        for ev in [Ev::Answer { by: User }, Ev::Escalate, Ev::Submitted, Ev::Dismiss, Ev::Close(CloseWhy::Unblocked)] {
            no(over.clone(), ev, Refused::Closed(CloseWhy::AnsweredElsewhere));
        }
    }

    #[test]
    fn the_wire_keeps_its_words() {
        let mut set = QuestionSet::default();
        sync(&mut set);
        assert_eq!((set.status.as_str(), set.decision.as_deref(), set.error.as_deref()), ("open", None, None));
        apply(&mut set, &Ev::Decide { auto: false }).unwrap();
        assert_eq!((set.status.as_str(), set.decision.as_deref()), ("open", Some("escalate")));
        apply(&mut set, &Ev::Answer { by: Answerer::User }).unwrap();
        assert_eq!(set.status, "answering");
        apply(&mut set, &Ev::Stopped { error: "no tab 3".into() }).unwrap();
        assert_eq!((set.status.as_str(), set.error.as_deref(), set.decision.as_deref()), ("failed", Some("no tab 3"), Some("escalate")));
        apply(&mut set, &Ev::Answer { by: Answerer::User }).unwrap();
        assert_eq!((set.status.as_str(), set.error.as_deref()), ("answering", None), "trying again clears the error");
        apply(&mut set, &Ev::Submitted).unwrap();
        assert_eq!(set.status, "answered");
        assert_eq!(apply(&mut set, &Ev::Answer { by: Answerer::User }), Err(Refused::AnsweredAlready));
        assert_eq!(set.status, "answered", "a refused move changes nothing");
    }

    #[test]
    fn old_words_still_load() {
        let s = SetState::from_wire;
        assert_eq!(s("open", None, None), st(Open, Deciding));
        assert_eq!(s("open", Some("escalate"), None), st(Open, Escalated));
        assert_eq!(s("answered", Some("auto"), None), st(Answered, Auto));
        assert_eq!(s("queued", Some("escalate"), None), st(Queued, Escalated));
        assert_eq!(s("failed", Some("escalate"), Some("stopped at tab 1")), st(Failed { error: "stopped at tab 1".into() }, Escalated));
        assert!(matches!(s("answering", Some("auto"), None).phase, Failed { .. }), "typing doesn't survive a restart");
        assert_eq!(s("", None, None), st(Open, Deciding));
        assert_eq!(s("something-new", Some("maybe"), None), st(Open, Deciding), "unknown words: shown, undecided");
    }

    #[test]
    fn late_answers_say_how_it_ended() {
        assert!(CloseWhy::AnsweredElsewhere.late("q1", "the orb").starts_with("already answered"));
        assert!(CloseWhy::Unblocked.late("q1", "the orb").starts_with("already answered"));
        assert!(CloseWhy::AnsweredHere.late("q1", "the orb").starts_with("already answered"));
        assert!(CloseWhy::Replaced { by: "q2".into() }.late("q1", "the orb").contains("changed"), "the phone's `changed` code");
        assert!(CloseWhy::SessionGone.late("q1", "the orb").contains("isn't there any more"));
    }
}
