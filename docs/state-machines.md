# State machines for the tower daemon (design)

Status: **implemented** on `tower-sm-impl` (see "As built" at the end for
where the code differs from this design). The sections below are the design
as proposed; the tables still describe the moves the code makes.

## Why

States are strings today, set and compared in many places:

| Entity | Where | Strings |
|---|---|---|
| Session status (Herdr) | `hc::status_word`, `daemon/watch.rs`, `asks.rs`, `sendq.rs` | `idle` `working` `blocked` `done` `error`, `""` (no status yet), plus the radar's own `gone` `exited` |
| Session state (hooks) | `daemon/sessions.rs` | `idle` `working` `waiting` `done` `ended` |
| Question set | `daemon/asks.rs`, `orb/panel/questions.rs`, Android `QuestionModel.kt` | status `open` `answering` `answered` `failed` `queued`; decision `None` `auto` `escalate` |
| Queued instruction | `daemon/sendq.rs` | no status field: derived from `parked`, `sending_ms`, `start`, `after`, plus `Delivered{taken, finished}`; park reasons `gone` `replaced` `after-cancelled` `start-failed` |
| Job | `history.rs` (folded from `history.jsonl`), `watch.rs`, `book.rs`, `main.rs` | `working` `blocked` `done` `failed` `error` `exited`, and `idle` (see below) |

What goes wrong with loose strings, seen in practice or in the code:

- **The same session has two vocabularies.** Herdr says `blocked` and the
  radar says `gone`, while the hooks say `waiting` and `ended`. Nothing maps one to the other.
- **A set's state is split across two fields**, `status` and `decision`, and
  each writer only checks the combinations it knows. For example, `escalate()` resets
  `failed`/`answering` to `open`, but `upsert()` keeps `answering`/`answered`
  only when the id is unchanged.
- **The outside world changes a state with nobody in the daemon making the transition.**
  - A dialog answered in the terminal.
  - A session that ends.
  - A set re-read under a new id.

  Each of these had its own ad-hoc fix: `close_where` from the radar, and
  today's `replaces` and "already answered".
- **Jobs record the session status raw.** `fire_watch` writes `idle` while
  `reconcile` maps `idle` to `done`. So the same finish can be `idle` or `done` in
  the history, depending on whether the daemon was up when it happened.

## Principles

1. **One enum per entity, one function per transition table.** Write
   `fn next(self, ev: Event) -> Result<State, Refused>`. Callers send events and
   never assign a state directly. A refused transition is a value the caller
   handles, not a silent no-op.
2. **Keep the wire format.** Use `#[serde(rename_all = "kebab-case")]` so that
   `state.json`, the phone API and the Android app see the same strings as
   today (`open`, `answering`, ...). Unknown strings read from disk or older
   clients map to a safe default; nothing panics.
3. **Herdr is the source of truth for anything outside the daemon.** On
   restart, and whenever a session's facts disagree with the stored state,
   the state is derived again from Herdr (screen, status, turn count,
   `created_ms`). Stored state only adds what Herdr can't say: decisions,
   what was promised, and what is owed.
4. **Every entity is tied to one terminal.** It stores the session's address and that
   terminal's `created_ms`. An address that comes back for another terminal is
   a different session (the queue and the watches already do this; question
   sets don't).
5. **Externally resolved is a state, not an error.** Answered elsewhere, gone
   and replaced each end a machine on purpose, and a late action on the old
   state gets a clear outcome, not "it failed".

---

## 1. Session status

Two sources feed it today: Herdr's status word, read by the radar from
`hc events` and `hc ls`, and the agent's hooks (`sessions.rs`). Both stay,
behind one type.

```rust
/// What a session is doing, as the tower understands it.
pub enum SessionStatus {
    /// Live, but no status reported yet (starting, or a first-run trust question).
    Starting,
    Idle,
    Working,
    /// Waiting on a person: a dialog (question, permission, plan, menu) or other.
    Blocked { kind: BlockKind },
    Done,
    Error,
    /// The terminal is there; its program ended (`state != "live"`).
    Exited,
    /// Herdr doesn't know the address any more (or it is another terminal now).
    Gone,
    /// A remote machine that doesn't answer: not gone, unknown.
    Unreachable,
}
pub enum BlockKind { Question, Permission, Other }

impl SessionStatus {
    pub fn ready(&self) -> bool  { matches!(self, Idle | Done) }        // sendq::ready
    pub fn passing(&self) -> bool { matches!(self, Working | Blocked { .. }) } // sendq::passing
    pub fn stopped(&self) -> bool { matches!(self, Idle | Done | Error | Blocked { .. }) }
}
```

| From | To | Event (today) |
|---|---|---|
| (none) | Starting / any | `created` event or the first snapshot (`Radar::event`, `observe`) |
| Starting | Blocked{Other} | trust check sees "trust" on screen (`trust_check`) |
| any live | Idle/Working/Blocked/Done/Error | `status`/`turn` event: re-read with `hc::lookup` (`Radar::event`) |
| any | Exited | `state != live` in a lookup or snapshot (`observe`: a watch reports `exited`) |
| any | Gone | `finished`/`removed`/`lost` event (all three go to `Radar::gone`, which reports a watch as `exited`), missing from a resync (`drop_missing_local`), or `created_ms` differs (`observe`: "a new terminal now") |
| any remote | Unreachable | `hc::Lookup::Unreachable` |

The hook states fold into the same type: `idle`→Idle, `working`→Working,
`waiting`→Blocked{Permission}, `done`→Done, `ended`→Exited. The radar should
take whichever source is newer, so it gets one answer instead of two strings.

**What survives a restart.** `watches.json` (`book.rs`) keeps each watched
session's last status and every agent's last status (`seen`). `sessions.json`
keeps the hook view. On restart, `reconcile` compares these with Herdr's
snapshot and owes a report for anything that stopped or ended meanwhile. That
stays as it is; only the strings become `SessionStatus`.

**Stale cases.** A turn that ended unseen is caught by the turn count
(`turned`). An address reused for a new terminal is caught by `created_ms`. A
missed event is caught by the sequence gap, which triggers a resync. All of
this exists already; the enum only makes the comparisons
(`was == "working" && status != "working"`) typed.

---

## 2. Question sets (fix first)

Today a set holds `status` (`open`, `answering`, `answered`, `failed`,
`queued`) and `decision` (`None`, `auto`, `escalate`) as separate strings. It
lives only in memory (`Asks::inner.sets`), as do `noted`, `dismissed` and the
waiting text replies (`replies`).

### Proposed type

```rust
pub struct QuestionSet { /* id, session, created_ms (new), kind, tabs, ... */ pub state: SetState, /* … */ }

pub enum SetState {
    /// Read from the screen but not whole yet (a Claude dialog before its
    /// transcript has the other tabs). Not shown, not told to the brain.
    Reading,
    /// Shown to the brain; it hasn't said yet (40 s, then Escalated).
    Deciding,
    /// The tower answers it itself (the brain said `auto`).
    Auto,
    /// The user's to answer (`escalate`, by the brain, the rules or a timeout).
    Escalated { reason: String },
    /// Keys going in now (one at a time: `start_answering`).
    Answering { by: Answerer },          // User | Tower
    /// Submitted; the dialog hasn't gone yet (the radar closes it).
    Answered,
    /// A text question's reply, waiting for the session to stop working.
    Queued { reply: Reply },
    /// Typing stopped part-way; the user may try again.
    Failed { error: String },
    /// The user hid it while the dialog is still up.
    Dismissed,
    /// Over: the dialog went away. Why, for a late action on it:
    Closed { why: CloseWhy },
}
pub enum CloseWhy { AnsweredHere, AnsweredElsewhere, Replaced { by: String }, SessionGone, Unblocked, TimedOut }
```

On the wire, `status` stays a string and `decision` stays where it is:
Deciding/Auto/Escalated keep `"open"` with decision `None`/`auto`/`escalate`,
and Closed{..} sets are removed as today. So the panel and the Android app
don't change in step 1.

### Transitions

| From | To | Event | Today |
|---|---|---|---|
| (none) | Reading | the radar sees the session blocked with a new status line | `seen` → spawned thread |
| Reading | Deciding | the set is whole (`exact`), or ~8 s passed | the re-read loop in `seen` (today's fix), then `refresh` → `upsert` |
| Deciding | Escalated | the brain says `escalate`, the 90/10 rule or policy says so, or no word in 40 s | `recommend` + `questions::decide`, the timer in `upsert` |
| Deciding | Auto | the brain says `auto` and the rules allow it | `recommend` |
| Auto | Answering{Tower} | auto-answer starts | `auto_answer` → `answer_as(auto)` |
| Auto / Answering{Tower} | Escalated | the screen now needs the user (`Held`), or typing failed | `escalate` |
| Escalated / Failed | Answering{User} | the panel, phone or CLI answers | `answer_as` → `start_answering` |
| Answering | Answered | submit went through (or a single-question dialog) | `set_status("answered")` |
| Answering | Escalated | answers filled without submit | `set_status("open")` |
| Answering | Failed | the driver stopped | `set_status("failed")` |
| Escalated (text) | Queued / Answered | reply typed now, or waiting for idle | `reply` |
| Queued | Closed{AnsweredHere} | the session goes idle/done and the reply goes in | `seen` → `due_reply` |
| Queued | Closed{SessionGone/TimedOut} | gone, too old (`REPLY_TTL_MS`), or the session moved on | `due_reply` → `Reply::stale` |
| any open | Dismissed | the user dismisses | `dismiss` |
| any but Answering | Closed{Replaced{by}} | the same dialog read again under a new id (same session, kind, tab titles) | `upsert` with `questions::redrawn` (today's fix). Today it replaces even an Answering set: the new rule waits for the typing to end |
| any | Closed{Unblocked / AnsweredElsewhere} | the radar sees the session not blocked; or an answer finds no dialog | `seen` → `close_where`; `answer_as` "already answered" (today's fix) |
| any | Closed{SessionGone} | the session ended | `Radar::gone` → `seen(.., "")` |

Refused transitions, each a value the caller turns into a message:
- **Answer an Answering set:** "already going in" (`start_answering`).
- **Answer a Closed set:** it is AnsweredElsewhere / Replaced / SessionGone,
  and the caller says that instead of "no question it can answer".
- **Answer a Reading set,** or a tab with no question: "not read yet" (today's fix).

### Restart

Today nothing is stored. The first `hc events` snapshot calls `observe` for
every terminal, and `seen` re-reads every blocked dialog, so the cards come
back (verified in the code path). What is lost:

- **The brain's decision and recommendations.** The rebuilt set is new
  (`noted` is empty), so the brain is told about it again, and an escalated
  question is announced to the user again.
- **`dismissed`:** a dialog the user hid comes back.
- **`replies`:** a text answer waiting for its session to stop is dropped
  without a word. The user was told "it goes in when it stops".

Proposal: keep `questions.json` with
`{id, session, created_ms, state, decision, reason, recommendations, replaces}` for open
sets, plus the waiting replies. At start, after the snapshot:
- If the session shows the same id on the same terminal, restore its
  decision, recommendations and dismissal.
- If it shows another id that is a redraw, it becomes Replaced, with what
  still fits carried over (as `upsert` does now).
- Otherwise it is Closed{Unblocked/SessionGone}: dropped quietly.
- A waiting reply whose session is still on the same terminal waits again,
  under the same staleness rules. Otherwise the user is told it didn't go in.

---

## 3. Queued instructions (`tower queue`, `dispatch`)

Already persisted (`queue.json`, written before each answer and each
hand-over) and already recovered at start. The state is implicit across
fields, so the step here is to name it, not to change behavior.

```rust
pub enum ItemState {
    /// A session to start once `after` is done (`start` set, `session` empty).
    ToStart,
    /// Waits for other ids to be done first.
    WaitingFor(Vec<u64>),
    /// Waits for its session to be ready (idle/done, gap and take rules).
    Waiting,
    /// Claimed on disk before typing; `turn_before` decides after a crash.
    Claimed { since_ms: u64, turn_before: Option<u64> },
    /// Won't go on its own; the brain was told once.
    Parked(ParkWhy),
}
pub enum ParkWhy { Gone, Replaced, AfterCancelled, StartFailed }

pub enum DeliveryState { Sent, Taken, Finished }   // Delivered{taken, finished}
// Removed from `items`: Delivered (moved to `delivered`) or Cancelled (id in `cancelled`).
```

| From | To | Event | Today |
|---|---|---|---|
| (new) | Waiting / WaitingFor / ToStart | `tower queue` / `dispatch` | `add_with`, `add_start` |
| (new) | Delivered | session ready and nothing ahead of it: sent at once | `add_with` → `work` |
| WaitingFor | Waiting | every `after` id finished | `after_done` |
| WaitingFor | Parked(AfterCancelled) | an `after` id was cancelled | `deliver`, `start_round` |
| ToStart | Delivered / Parked(StartFailed) | started, or `tower start` failed | `start_round` |
| Waiting | Claimed | the session is ready and may take it (`may_send`) | `deliver` |
| Claimed | Delivered(Sent) | `hc send` succeeded | `went_in` |
| Claimed | Waiting (tries+1) | `hc send` failed | `deliver` |
| Claimed | Delivered / Waiting | at start: did the turn count move? | `recover` |
| Waiting / WaitingFor | Parked(Gone) | the session is exited or gone | `session_round` → `park` |
| Waiting / WaitingFor | Parked(Replaced) | same address, other `created_ms` | `session_round` |
| Parked | Waiting | `tower queue retarget <id> <session>` | `retarget_with` |
| Waiting / Parked | Cancelled | `tower queue cancel` (refused while Claimed) | `cancel` |
| Sent → Taken → Finished | | turn count moved; then the session was ready again; or the session ended | `follow`, `session_round` |

**Stale and external cases are handled:**
- Unreachable machines keep waiting; the brain hears after 10 min.
- A session in `error` keeps its instructions, and the brain hears it once (`stuck`).
- A retry within 10 min is the same item.
- A session gone or replaced parks its items.

**Gaps:**
- A parked item never expires. It stays in `tower queue list` until someone
  cancels or retargets it. That is the "orphaned for gone sessions" case:
  correct (never dropped silently), but it piles up.
- `park` skips a Claimed item. That is right while it is being typed, but a
  claim left by a crash is only cleared at the next start.

Proposal: after N days parked (say 7), the brain is reminded once and the item
moves to an `expired` list kept beside `delivered`, without deleting it.

---

## 4. Jobs

Event-sourced: each change is a `job` line in `history.jsonl`, and
`history::jobs` folds them (the last `status` wins). So everything survives a
restart.

```rust
pub enum JobState { Working, Blocked, Done, Failed, Error, Exited }
impl JobState { fn open(&self) -> bool { matches!(self, Working | Blocked) } }
```

| From | To | Event | Today |
|---|---|---|---|
| (new) | Working | `tower watch` (`Watches::add`), `tower start`, `tower job start` | `job_start_on` |
| Working | Blocked | a watch fires on an unhandled block; the trust check | `fire_watch` → `job_watched`, `trust_check` |
| Working / Blocked | Done / Error / Exited | a watch fires on a stop or exit; startup `reconcile` | `job_watched`, `job_update` |
| any | Done / Failed | `tower job done` / `tower job failed` | `main.rs` |

**Gaps:**
- **Raw statuses are recorded.** A watch that fires on an idle session writes
  `idle`, but `reconcile` writes `done` for the same thing. With an enum,
  `SessionStatus` maps to `JobState` in one place: Idle/Done→Done, Exited/Gone→Exited.
- **Blocked never goes back.** A job that went Blocked (the watch fired and is
  consumed) is never moved back to Working or on to Done when the session
  carries on. It stays open until a restart's `reconcile` or the brain's
  `tower job done`.
- **Unwatched sessions never move their job.** A job started with no watch
  (`tower start --no-watch`, `tower job start`) changes only by hand.

Proposal: the radar already sees every status change. For a session with an
open job, it records the job transition itself (Blocked→Working,
Working/Blocked→Done/Error/Exited), and the watch only adds the report to the brain.

---

## Migration plan (safe steps)

Each step is its own commit, behavior-preserving except where noted, with the
wire strings unchanged.

0. **Commit the open question-card fixes first** (uncommitted on
   `tower-hooks-and-fixes`):
   - the `│` gutter and wrapped questions;
   - tab numbers counted from 0;
   - showing a dialog only once it's read whole;
   - `replaces` plus carrying a card over;
   - "already answered";
   - unread tabs.

   They are the behavior step 1 must keep. Their tests (`questions.rs`,
   `asks.rs`, `panel/questions.rs`) become step 1's regression suite. The
   voice-on-Mac setting is unrelated and can be committed separately.
1. **Question sets: the enum, in memory.** Add `SetState` and
   `fn next(&self, ev)`, and route every write in `asks.rs` through it:
   `upsert`, `escalate`, `start_answering`, `set_status`, `reply`,
   `close_where`, `dismiss`. Keep serializing `status`/`decision` exactly as
   today. Add a table test that walks every row above, including the refused
   transitions and the three external cases (answered in the terminal,
   replaced, gone).
2. **Question sets: persist** (`questions.json`, tied to `created_ms`), and
   restore at start as described above. This fixes re-telling the brain and
   re-announcing after a restart, dismissed cards coming back, and waiting
   text replies being dropped. Test: restart with a fake `detect`.
3. **`SessionStatus`** in `hc.rs`, with `status_word` returning it. Replace
   the string matches in `watch.rs`, `asks.rs` and `sendq.rs` (`ready`,
   `passing`, `stopped`). Fold the hook states into it. This is mechanical,
   and the compiler finds every use.
4. **Jobs:** add `JobState`, normalize what is recorded (`idle`→`done`), and
   let the radar move open jobs on status changes. The panel and Android
   chat filters (`done|failed|error|exited|blocked`) keep working.
5. **Queue:** add `fn state(&Item) -> ItemState` (read-only first), use it in
   `describe`, `waiting_for` and `sessions`, then add expiry for parked items.
   Storage is unchanged.

Step 1 is small (one file plus tests) and directly hardens the cards that
broke today. Steps 3–5 are independent of each other.

---

## As built

| Entity | Type and table | Stored as |
|---|---|---|
| Question set | `daemon/card.rs`: `SetState { phase, decision }`, `SetState::next(&Ev)`; `card::apply` is the only writer of `status`/`decision`/`error` | `questions.json` (new): the sets' wire words, plus `created_ms`, dismissed dialogs, waiting replies, `noted` |
| Queued instruction | `sendq.rs`: `Item::state() -> ItemState`, `Item::apply(ItemEv)`, `ParkWhy`, `Delivered::state/take/finish` | `queue.json` as before; new optional `parked_ms`, `expired` |
| Job | `history.rs`: `JobState`, `JobState::next(&JobEv)`, `follow`/`job_follow` | `history.jsonl` as before (job words) |
| Session status | `hc.rs`: `SessionStatus`, `status`, `status_live`, `block_kind` | its word everywhere (`watches.json`, `sessions.json`, `questions.json`, the state's agents) |

Where it differs from the design above:

- **Question sets: phase and decision are two fields of one state**, not one
  flat enum. The decision has to outlive the phase it was made in (a card
  the user decides stays `escalate` while it is answered, fails, is tried
  again), and the wire keeps both. Deciding/Auto/Escalated are
  `Phase::Open` with `Decision::{Deciding, Auto, Escalated}`. The escalation
  reason stays in the set's `reason` field.
- **Reading** is held per session (`Inner::reading`: one reader per dialog
  screen), not as a set state: a set has no stable id until it is read whole.
- **Closed sets** are removed from the list as before, and remembered (last
  100, in memory) with why, so a late answer says how it ended. A late
  answer to a *replaced* card follows it to the screen and goes in when its
  tabs didn't change; a late answer to a *dismissed* dialog still up goes in.
  After a restart only the sets that closed at the restore are remembered.
- **Restart**: a restored set the brain never decided gets its 40 s wait
  again; one decided `auto` but not typed goes to the user (the tower doesn't
  type after a restart without a fresh look); one being typed comes back
  `failed`, to try again. Any dropped waiting reply (too old, moved on,
  gone, other terminal) is now told to the user, not only logged.
- **SessionStatus::Blocked has no kind inside**: the kind is read with
  `hc::block_kind`, so a status compares equal exactly when the old strings
  did. Unknown words are kept (`Other(String)`) rather than mapped away, in
  `SessionStatus`, `JobState` and `ParkWhy`, so nothing on disk is lost.
- **Jobs**: the radar moves an open job only on changes it sees while up
  (not on its first look at a session), so the start's `reconcile` still
  reports what happened while the tower was down before the job moves.
- **Queue**: parked items expire after 7 days into `expired` (kept,
  retargetable; `cancel` says it expired). One parked by an older version
  counts from when this version first sees it.
- Report and alert words (`Pending.status` with `news`, alert levels) are a
  different vocabulary and stay strings.
- **Testing**: `HERDER_TOWER_TEST=1` makes a daemon handle only test sessions
  (`qfix-…`, `tower.test`), the ones the real tower leaves alone (README,
  "How it fits together").

---

## 5. Agendas and their tasks (`src/agenda.rs`, `src/daemon/agendas.rs`)

The ledger above the other machines: one **agenda** per request of the
user's (their words, the spaces it touches), one **task** per instruction
it went out as, tied to the send queue's ids (a retry, a re-prompt or a
follow-up adds an id to the same task). `dispatch`, `queue` (`add`,
`start`) and `start` (`track`) register in the daemon as the queue takes
the instruction; what goes out while the user's request is under half an
hour old joins its agenda, more for a session whose task waits on a
question is that task again, and work the brain starts on its own gets an
agenda with `origin: tower`.

```rust
pub enum TaskState { Queued, Running, Blocked, DoneClaimed, Verified, Failed, Stalled, Abandoned }
pub enum TaskEv { Delivered{..}, Working, Blocked{question}, Claimed{said}, Verify{how}, Fail{why}, Stall{why}, Requeue{queue_id, text}, Abandon{why} }
pub enum AgendaState { Open, Done, Closed, Abandoned }
```

| From | Event | To |
|---|---|---|
| queued, stalled | delivered (the queue's id went in; a new session's first task) | running |
| queued, running, blocked, stalled | the session works (Herdr, hooks) | running |
| any open | the session is blocked (dialog), or its turn ends asking the user | blocked |
| any open | its turn ends with a message (Stop hook); another instruction went in after it | done-claimed |
| done-claimed | the message says it's done and names what it was asked | verified |
| any open | `tower job done`, `tower agenda verify` | verified |
| any but verified, abandoned | it says it couldn't, StopFailure, `tower job failed`, `tower agenda fail` | failed |
| any open | parked / expired in the queue, the session exited, vanished or is another terminal, error, never took it after a re-send, no progress 25 min, an id the queue has no trace of | stalled |
| any | `tower agenda requeue`, the reconciler's re-send, a follow-up, a parked id retargeted | queued |
| any but verified | the queue id cancelled, `tower agenda abandon`, `close --force` | abandoned |

Refused moves are values (`Refused::Same`, `Settled`, `NotNow`): once a
task is failed, verified or abandoned, what its session does next is the
next task's. An agenda is `done` by itself once every task is verified or
abandoned (and open again if one is requeued); `closed` and `abandoned`
are said by hand, and `close` refuses while tasks are unfinished unless
`--force`.

**The reconciler** runs every 20 s and at once on any hook event or
registration. Facts are gathered outside the ledger's lock (the queue's
`fact(id)`, one `hc` lookup per session, the hooks registry, the open
question cards, and, for a task working a while with no other sign of
life, a look at its screen with the digits left out); `Ledger::step`
applies `assess` to each open task; `Ledger::news` says what the brain
should hear: a task in trouble once per state and reason, after a grace
(45 s; 3 min for an unverified claim, since the brain usually judges it
from the watch report; 30 min for a question the user hasn't answered),
an agenda of two tasks or more when it is all done, and up to three
reminders, six hours apart, of what was told and is still unfinished.
Notes go through the radar's outbox as kind `agenda` (`News::Asked`: from
a space out of focus they come through, marked).

**Restart**: the ledger is written whole after every change (a synced
temporary file, then a rename); an unreadable file is kept aside. At start
the reconciler waits for the radar's and the queue's own reconcile, then
looks at every open task again: what ended or finished while the voice was
down moves then.
