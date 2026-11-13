# Quiet Tower

The tower does the work and keeps the record. It speaks in two cases: the user
asked, or the decision is the user's. Everything else it writes down.

This file is the contract for that change. The design, with every screen and
state drawn (codes like F1, W5, C3, A2 below refer to its "Every state"
gallery), is the artifact "Quiet Tower"; its source is `docs/quiet-tower.html`.

Measured on 10 Nov 2026, from `history.jsonl`: 8 requests from the user, 277
messages back, 111 of them to say nothing was needed; 112 "waits for you"
alerts, 28 from sessions with no task; 50 of 126 queued instructions were one
session writing to another, each filed as a task, watched, reported to the
brain and narrated.

## Three lanes

| Lane | What | How it reaches the user |
|---|---|---|
| Yours | A decision only the user can make: merges, deploys, deletes, production data, secrets, money, design choices, a task that failed twice | One card per decision, the orb's amber count, one spoken sentence (if voice is on) |
| Asked | The answer to something the user said or typed | One reply, where they asked. A command gets a receipt, never a question |
| Record | Everything else: starts, deliveries, finishes, messages between sessions, routine answers, retries | Written to the record. Never spoken, never pushed |

## One setting: how much it tells you

`config.toml`:

```toml
[tell]
level = "decisions"   # decisions | results | everything
```

- `decisions` (the default): only the Yours lane and direct replies reach the user.
- `results`: also one receipt when a request the user made is finished.
- `everything`: today's behaviour, unchanged. Every step below keeps this
  level working as before, so the user can always go back.

`tower tell [decisions|results|everything]` reads or sets it (kept in
config.toml, read live by the daemon; `tell_level` in the state). A word
config.toml doesn't know is `decisions`. Setting it also writes the brain's
instructions again and tells the brain to read them, because they say what
a note is at that level; an edit of config.toml by hand reaches the daemon
at once and the brain at its next start (or `tower tell <level>`).
`tower install` tells a running brain to read its instructions again when
they changed.

### What it says out loud

A second word in the same table says what is spoken (the design's Voice
setting, W17):

```toml
[tell]
voice = "replies"   # off | decisions | replies
```

- `off`: nothing. No words, no bell, no chimes. It is the mute.
- `decisions`: a decision, once, in one sentence (a question a session
  asks, a wait, a merge, what was raised, a task that failed twice, a
  session blocked or stuck on an error). Nothing else: no answer, no
  "Okay.", no receipt, no other news. The microphone's own two tones still
  answer the talk key.
- `replies` (the default): its answers to what the user asks too, with
  their acknowledgement, and whatever else the level lets through (a
  receipt at `results`, the news at `everything`). This is the tower as it
  spoke before the setting. An answer is spoken whether the request was
  spoken or typed (the design says "when you spoke to it"; the tower has
  always answered a typed request aloud, and that is kept).

`tower tell --voice off|decisions|replies` sets it (alone, or with a
level); `tower tell` prints both (`{"level", "voice"}` when piped). It is
kept in config.toml and read live: a hand edit reaches the mouth within two
seconds. `tower mute on` is `voice = "off"`, and `tower mute off` goes back
to what it was before the mute (`decisions` stays `decisions`; kept in the
mute file `muted` in the state folder, so past a restart too). The state
has `tell_voice` (`off`, `decisions`, `replies`) beside `muted` (true
exactly when it is `off`). The socket: `{"cmd":"tell","voice":"decisions"}`
(with or without `level`); the answer is `{"level", "voice"}` plus `"was"`
and `"voice_was"` for what was set.

What each sound is, is said where it is made (`tell::Spoken`: `Reply`,
`Ack`, `Decision`, `Receipt`, `News`, `Chime`) and asked of the setting
when its turn comes (`tell::Voice::speaks`), so a sentence that waited
while the setting changed is not said after all. What is not said still
shows (`said` in the state), as while muted. "Always on this Mac"
(`tower mac-voice`) is a switch of its own: where speech plays, not whether.

Per space: **follow** (that space's results reach the user as they land) and
**mute** (its decisions stay in its own tower: off the orb's count and out of
the main queue). `tower space follow|unfollow|mute|unmute <space>`.

Both are kept in `spaces.json` as lists of space names (`follow`, `mute`;
left out while empty), and `tower spaces` shows them per space (`follow`,
`mute`). `spaces::followed`, `spaces::muted_in`. A name is matched in any
case. A muted space's decisions are also not said (step 3).

What "written to the record" means in the history (`history.jsonl`): an
entry with `"record": true` is not a chat message (the panel and the phone
use one rule, `chat::shown`) and was never spoken. It is decided when the
entry is written, so changing the level never changes what the chat showed
before.

## Step 1: note turns are silent

A brain turn whose prompt is a note from the tower itself (`[watch]`,
`[queue]`, `[agenda]`; `brain::is_note_prompt`) is a *note turn*.
(`brain::is_news_prompt` keeps its old meaning, `[watch]` and `[queue]`
only: at `everything` it still decides which words come after the bell.)

At level `decisions` or `results`:

- What the brain writes on a note turn is not a chat message and is not
  spoken (its text blocks as it writes them, its final message, and a
  `tower say` from its own terminal). The final message is kept in the
  history as kind `note` with `record: true`, and with the session, name
  and space of the session the note names (the first address in it; a note
  about no one session has none), so the record has it.
- The brain puts something in front of the user only by a command:
  - `tower raise <session> --why "<category>" "<the decision, one line>" [--options '<json>'] [--detail "<text>"]`
    makes a decision (step 3). `--why` is required; `--options` is a JSON
    list of `{"label","why","text"}` (or plain words), the recommended one
    first; `--detail` is what the line can't say (the command it would run,
    the plan, the change), the decision's `detail`, cut to about 600
    characters with an ellipsis (raised again with one, the card shows the
    newer). It writes a history entry of kind `raised` (`id` = the
    decision's, `session`, `name`, `space`, `why`, `text`, `options`;
    `record: true` when it was raised before) and, the first time, the
    alert a session that needs the user makes ("<name> needs you: <text>.
    I'd say: <pick>", shown, said once, held by the focus like any such
    alert). Raised again it is the same decision and says nothing.
  - `tower receipt [--agenda <id>] "<one line>"` is the single receipt for a
    finished request. It is shown (a tower message in the chat) and spoken
    at level `results` (and `everything`), or when one of the request's
    spaces is followed; otherwise it goes to the record. History kind
    `receipt` (`text`, `agenda`, `space`, and `record: true` when it was
    only kept). With `--agenda` (an agenda's id, or one of its tasks') it
    is refused while that agenda has open tasks, and a second time
    (`receipt_ms` on the agenda); the receipt of work the tower started on
    its own (the agenda's origin is `tower`) is told only for a followed
    space. Without `--agenda` the tower can't tell the space, so only the
    level decides. `tower job done` on a session says when that finished a
    request of the user's (within the last half hour, no receipt yet), with
    the command to give it.
- `[queue] delivered …` notes are not sent to the brain at all; they go to
  the record (the `queue` entries the history already had). The same for
  `[queue] started …`. What is stuck (`is gone`, `is error`, `can't
  reach`, parked) still reaches the brain.
- "X is done" alerts are record entries, not chat items and not spoken.
  So are the other finishes: a watch's report unless the session is
  blocked or hit an error, a `tower job done` line, a question the tower
  answered by itself. What needs the user (`blocked`, `error`, `failed`)
  is shown at every level.
- Every note the brain gets at these levels ends with one sentence saying
  it is for the record and how to answer it (`tell::NOTE_TAIL`), since the
  notes' own words still say "tell me what happened".
- The fixed core prompt (`assets/tower-core.md`) and the built-in behaviors
  say this: a note is news for the record; answer it only by acting
  (`tower job done`, `tower agenda verify`, `tower raise`, `tower receipt`);
  write nothing for the user. The lines "Tell the user what happened" and
  "Never answer one with silence toward the user" go. They go at these two
  levels: the brain's instructions are written for the level in force
  (lines of the core that start with `{quiet}` or `{everything}` are for
  those levels only), so at `everything` the brain still reads the old
  lines and tells the news as before.
- A `[voice]` turn (the user asked) is unchanged: its answer is a chat
  message and is spoken.

## Step 2: sessions talk without the tower in between

- `tower queue` records who called it: `from` = `HC_TERMINAL_ID` of the
  caller (as `tower screenshot` finds its session), carried in
  `Request::Queue`. `tower dispatch` and `tower start` send none: what they
  hand out is always work.
- An instruction whose sender is an agent session other than the brain
  (the terminal named `tower`, or a space tower `tower-…`) is a **peer
  message**. An agent session is a terminal Herdr reports an agent for
  (Claude Code, Codex); a plain shell, the panel, the phone and
  `tower-voice` are the user's or the tower's, so an instruction as before.
  A brain is known by its own name in Herdr, the one it was started with
  (`brain::brain_of`): a terminal started by hand as `tower-…` counts as
  one. A session in a space's tab has no name of its own, only the label
  `tower.name`, and is never a brain whatever it is called; `tower start`
  refuses to call a session `tower` or `tower-…`, and `tower launch` names
  a session `<repo>-<space>` where `<space>-<repo>` would be such a name.
  This holds at `decisions` and `results`. At `everything` a session's
  instruction is handled as it was (a task, watched, told to the brain);
  it only gains the sender's name, and its task records who sent it
  (`from`), so it is never shown as a request of the user's. A peer
  message is:
  - delivered with the same queue guarantees (waits for a busy receiver, in
    order, once, kept across restarts), but for a receiver busy a long
    while (below);
  - the daemon puts the sender's name on it ("From <space> › <name>
    (<address>): …") unless the text already starts with "From ";
  - no agenda task, no job, no watch for the brain, and no `[queue]`,
    `[watch]` or `[agenda]` note to any brain about it;
  - recorded in the history as kind `peer` when it arrives (`session` and
    `name` = receiver, `from` and `from_name` = sender, `space` = the
    receiver's, else the sender's, `status` = `delivered`, `text` as the
    sender wrote it, `record: true`).
- **A receiver that works for hours.** A Claude Code session that runs
  background agents stays `working` for hours, and messages to it from
  other sessions used to wait all that while. A peer message whose
  receiver is a Claude Code session (not a brain) that has been working
  longer than `[queue] busy_deliver_minutes` is typed in anyway:

  ```toml
  [queue]
  busy_deliver_minutes = 10   # 0: never; a peer message waits for the turn to end
  ```

  How long it has worked is Herdr's `status_ms` (when it set the status),
  or the queue's own first sight of it working at its turn count, whichever
  is longer; the setting is read live. The tower types into a session with
  `hc send`, which pastes the text and presses Enter, nothing else (no Esc,
  no interrupt), here with `--if working`, so hc looks again as it types
  and refuses a session that stopped or is blocked on a dialog meanwhile
  (it refuses a dialog anyway). Claude Code keeps what is typed while it
  works and sends it as its next message when its turn ends: nothing is
  interrupted. The bookkeeping says it went in once:
  - One at a time: the next message waits until the session took the last
    one (its turn count moved past the turn that was under way) or has been
    free for 90 seconds without starting on it (then it went into that
    turn). Peer messages keep their order among themselves; one typed in
    this way goes ahead of instructions that wait for the session, which
    go in as before once the session is free (after the turn on the
    message, too). The user's own instructions, a brain's, an answer from a
    decision, an introduction: never typed into a working session.
  - It is claimed on disk before it is typed, with the turn count that
    turn will end on. A restart that finds it claimed reads the count: the
    session took a turn past that one, so it went in and is not typed
    again; else it is not typed in again while that turn lasts (it may be
    in the session's input already), only once the session is free, and
    then only if the count still says it wasn't taken.
  - What is typed in this way is the tower's for a day (`daemon::typed`),
    so its prompt, when it comes hours later, is no request of the user's
    and answers no question of theirs; the turn it was typed into stays
    whoever's it was. Its delivery is no "next hand-over" for the task the
    session was working on (the agenda's `next_ms` leaves it out), so that
    task's turn isn't taken as over.
  - It is in the thread as delivered when it is typed in; `tower queue`
    says `while_working: true`.
  - Codex sessions keep waiting: nothing here says Codex keeps typed input
    for its next turn, so a message to one waits for its turn to end, as
    before.
- **A turn that ended on an error.** A Claude Code or Codex session whose
  turn ended on an API error has status `error` (the radar says it "hit an
  error", "stuck on an error"), but it sits at its prompt: it takes what
  waits for it at once, as idle or done would (`sendq::Session::ready`),
  with no "is error" note to the brain. A terminal of another program in
  `error` still waits, and the brain hears it once, as before.
- A peer message that cannot be delivered (receiver gone, parked): the
  sender is told with one queued message, and it is recorded (`status` =
  `undelivered`). The user is not told. It is taken out of the queue, not
  parked; several lost messages from one sender to one receiver make one
  note. A receiver that isn't there when `tower queue` runs is the
  command's own error, as before.
- `tower thread [<space>] [--json] [-n 50]` lists the peer messages of a
  space, newest last (no space: every space's):
  `{"space", "messages":[{"at","from","from_name","to","to_name","text","space","delivered"}]}`.
  The state and the phone API carry the same thread per space: `state.json`
  has `threads`, by space name, the last 50 each with the text cut to 400
  characters. The phone's `state` (the call and the event) leaves `threads`
  out, since it is sent on every change; `GET /v1/thread` (step 11) serves
  it from the history, whole and in pages, and a new message is a `thread`
  event.
- The introduction text keeps `tower queue <address> "<message>"` and says
  the tower adds the sender's name.
- A session's message to a brain (`tower queue tower "…"`, or to a space's
  `tower-…`) goes in tagged: `[session] From <space> › <name> (<address>):
  <text>`, with one sentence saying how to answer it (`peers::for_brain`).
  `[session]` is a note tag (`brain::is_note_prompt`), so the brain's turn
  on it is a note turn: nothing it writes is shown or spoken, and its final
  message is kept as a `note` about that session. The thread keeps the
  message as the session wrote it.

## Step 3: what needs you is a decision

A **decision** is the one thing shown to the user in the Yours lane. The
model is `src/decisions.rs`; the daemon holds it in `src/daemon/decisions.rs`
and keeps it in `decisions.json` (the state folder) across restarts.

```
Decision { id, key, kind, space, sessions[], title, detail, why, options[], pick,
           prs[], question_id, task, first_ms, last_ms, count, state }
kind  = permission | question | merge | choice | failed | raised | blocked
state = open | answering | answered | stopped | dismissed
```

`id` is `d` and 12 hex digits (a question set's is `q…`). It belongs to
this decision from when it opened: the same thing needing the user again,
after it closed, is a new decision with a new id.

`detail` is what the title can't say, to read before deciding, cut to about
600 characters with an ellipsis (`decisions::DETAIL`): for a question or a
permission its question set's `detail` (the command a permission or a Codex
approval would run, the plan to approve; for a question at the end of a
turn the message that asks, its end kept, since it asks last); for `tower
raise` its `--detail`; for a merge none (its PRs say it); "" when there is
none. It is in `state.json`, `tower decisions --json`, `GET /v1/decisions`
and the `decisions` event. The Mac's cards show it in the small monospace
block a question's card uses for its dialog's detail: a card with no
question set behind it (a raise) has that block now too.

### Where they come from

| Source | Kind | Key | Options |
|---|---|---|---|
| A question set that is the user's (`decision: escalate`; a "waits for you" card is one) | `permission` for a permission, plan or approval menu; `question` otherwise | `ask:<space>:<what it asks>` | The tower's recommendations, the first one the pick (for a set with several questions, option *i* answers each with its recommendation *i*); with none, a menu's own options and no pick. A wait card, and a question after a turn the user typed, gets "Go ahead" and "Not yet" at once and then two or three of its own (`crate::choices`) |
| The PRs of one repo that wait for a merge in one space (`waiting` and open in `prs.json`) | `merge` | `merge:<space>:<owner/repo>` | One per PR, in order. The pick is the first, when it can be merged, its checks pass and its base is not production |
| `tower raise` | `raised`; `choice` with `--options` | `raise:<space>:<the line>` | As given, the first one the pick |
| A PR's review stuck after three rounds ("PR review") | `choice` (`why` `review`), about the owner and the reviewer | `raise:<space>:<its line>` | "Side with the owner", "Side with the reviewer", "I'll look"; no pick |
| An agenda task that failed again after a retry | `failed` | `failed:<space>:<task id>` | "Try again", "Drop it"; no pick |
| Sessions a restart of the Mac or Herdr closed ("After a restart") | `choice` (`why` `reopen`), in no space and naming no session | `reopen` (one at a time) | "Reopen all" (the pick; "Reopen it" for one), "Only <space>" for each space when there are several (up to three), "Forget them". The tower carries the answer out itself: nothing is sent to a session, and it is never made a rule |
| A session blocked on something the tower can't read (no question set comes of it: no menu on its screen; a dialog with numbered options is a question set, even one it doesn't know by its question) | `blocked` | `blocked:<space>:<address>` | None. It is answered in the session itself ("Show in Herdr"); it closes by itself, as `answered`, once the session is no longer blocked, and as `stopped` when the session is gone. Told once at a quiet level, as the plain notice was; every time at `everything` |

- Text in a key is normalised: lowercased, every run of punctuation and
  whitespace one space, cut to 300 characters. What a question set asks is
  every question of the set; for a menu, its title and a hash of its detail
  (another command or plan is another decision).
- **One key per decision.** The same key again updates `last_ms`, adds
  one to `count` and adds the session to `sessions`; it never makes a second
  decision. Three sessions asking the same thing are one decision with
  `waiting: 3`. Each session keeps its own question set (`questions` is
  unchanged); the decision's `question_id` is its first session's, and
  each row of `sessions` has its own.
- A merge decision's `prs` are in order: a non-production base first
  (`prs::is_prod`: main, master, prod, production, release, live are
  production), then by number. It changes in place as PRs open, merge,
  close or their checks move; `count` goes up when another PR joins.
- A `tower raise` about something already in front of the user joins that
  decision instead of making one (the answer says `"new": false,
  "folded": true`): `--why merge` naming a PR number a merge decision of
  that space lists (or, naming none, about one of its sessions); `--why
  waiting|blocked|question` about a session with an open question
  decision; `--why failed` about a session with an open failed decision.
- **A failed task.** The tower did not retry by itself before: a failed
  task was told to the brain, which asked the user or ran `tower agenda
  requeue`. Now, at `decisions` and `results`, a task that fails (its
  session says it couldn't, its turn fails, or `tower job failed` / `tower
  agenda fail` says so) is sent to its session once more by the daemon,
  within ten minutes of failing ("That didn't work (<why>). Try it once
  more: …"), and the brain gets an `[agenda]` note saying so. One that
  failed longer ago (before a restart, under an older tower) is left as it
  is. On the task, `retries` counts tries after a failure (the daemon's, or
  `tower agenda requeue`) and `retried_ms` says when; a `tower job failed`
  that arrives within ten minutes of the retry, while the new try runs, is
  taken as word on the try before and changes nothing. A task that fails
  with `retries >= 1` becomes a `failed` decision (`raised` on the task),
  at every level; at the quiet levels the brain is told it is one and not
  to raise it. A retry that can't be sent (the session has ended) counts as
  tried. At `everything` nothing is retried by the daemon (the brain hears
  of the failure and asks the user, as before).
  The retry re-sends an instruction unasked, so it has a switch in
  config.toml, read live:

  ```toml
  [agenda]
  retry_failed = true   # false: the tower sends nothing again by itself
  ```

  With `retry_failed = false` a first failure is neither retried nor a
  decision: the ledger's `[agenda]` note tells the brain (as at
  `everything`), which raises it (`tower raise … --why failed`) or sends it
  again (`tower agenda requeue`); a task that fails after any retry is
  still a `failed` decision.

### Every card has buttons

A card the brain doesn't read used to come with nothing to tap: a turn
that ended waiting for the user ("waits for you", `questions::WAITS`) and
a question at the end of a turn they typed in the session's terminal
(`questions::TYPED_THERE`), and one escalated without answers. Such a text
set (one question, no recommendations, the user's) now gets them
(`Asks::give_buttons`, `crate::choices`), once:

- at once, "Go ahead" ("Go ahead with what you proposed.") and "Not yet"
  ("Not yet: hold off and wait for me.");
- a few seconds later, two or three of its own, read from what the
  session said by one headless call (the default account, `[brains] model`
  at low effort, no tools, nothing kept): a short label and the
  instruction it sends, the tower's pick first with why ("Push and open
  the PR", "Push only, no PR", "Not yet"). They take the generic ones'
  place while nobody is answering; when the call fails or brings fewer
  than two, the generic ones stay.

They are the set's recommendations for its one question, so the card's
rows, the decision's `options` (the phone's buttons) and the reply typed
into the session work as for any question the tower reads. "Other" stays
below; the decision stays the user's (nothing is answered by itself).

### When one closes

- A question's: when no set with its key is the user's any more. `answered`
  when one of its sets was answered (from the tower, or in the session),
  else `dismissed` when one was dismissed, else `stopped`. It is
  `answering` while an answer goes in (keys into a dialog, or a reply
  waiting for its session to stop).
- **Answered through a brain.** The user tells a brain (the main one or a
  space's own: the panel, the phone, the voice) their answer to a question
  a session asked at the end of its turn, and the brain passes it on with
  `tower queue <session> "…"` instead of `tower answer`. The queue types it
  in like any instruction, so it doesn't count as the user's own prompt;
  but when the caller is a brain, the user spoke to that brain after the
  question came (`brain::last_asked`), and the session has such a question
  that is the user's (it stands in a decision), one headless call
  (`covered::answers_question`, low effort, as `covered::judge`) reads
  whether the message answers it: gives the decision or the go-ahead, says
  what to do instead, or makes it moot (`Asks::relayed`). If it does, the
  set closes (`settled`) and its session leaves the decision, which closes
  `answered` with "answered through the tower: <the message>" when nobody
  else asks the same (`Decisions::passed_on`). A note of the tower's, a
  message from another session, a brain writing on its own, a message
  about something else, or no answer from the model leaves the question
  and its card open, as before.
- A merge's: when none of its PRs is open and waiting (`answered` when one
  was merged, else `stopped`), or when its space is closed or archived
  (`stopped`). Not when its sessions end: a PR that is still open and
  waiting is still the user's. A session that ends stays in its
  `sessions` with `waiting: false` (so `waiting` can be 0, and it sorts
  after those somebody waits on).
- A failed task's: when it is answered, or the task is no longer failed
  (sent again, verified or dropped by anyone): `answered`.
- Any but a merge: `tower answer`, `tower dismiss`, or when all its
  sessions are gone (`stopped`). A session that ends leaves its decisions.
- When a space goes quiet (its last session is gone) what was raised there
  and its failed tasks close (`stopped`), its merges stay; when it is
  closed or archived its merges close too (`Book::space_over`). The PRs of
  a merge closed that way, and of a dismissed one, make no decision again
  while they wait (`settled_prs` in `decisions.json`).
- A decision raised about a session that had already ended has no sessions
  and stays until it is answered or dismissed.
- The last 50 that closed are kept (`tower decisions --all`).

### Answered in the session's own terminal

The user often answers in the session itself, not on the card. The tower
stays in step:

- **A dialog** (a permission, Claude Code's questions, a plan, a Codex
  approval) that its session stops showing without the tower having typed
  its answer (the radar sees it no longer blocked, or showing another
  dialog) was answered there. Its question set closes, and the decision it
  stood in closes in the same step, `answered` (`Book::answered_in_terminal`;
  while other sessions still ask the same, only this one leaves it), so the
  state carries it at once: off the Mac's windows, a `decisions` event to
  the phone. Nothing is said. A moment later the record gets one line, a
  `decision` entry with `status: answered` and `source:
  terminal:<address>`: "<what it asked> -> Answered in <space › session>:
  <what was chosen>" when the screen shows it (Claude Code's "User
  answered Claude's questions", a plan approved or rejected, a permission
  turned down, Codex's "You approved codex to run …";
  `questions::chosen_on_screen`), else "… -> Answered in its terminal". The
  decision's `answer` is "in its terminal[: <what was chosen>]".
- **Nothing more is typed into it**: an answer going in stops at the next
  key (`Guarded`: the set is closed); an auto-answer the 90/10 rule decided,
  a recommendation that comes late, the 40-second escalation, a card
  answered from the phone a moment late: each finds the set over ("already
  answered") and types nothing. A menu read again is the same dialog only
  with the same command or plan (`questions::redrawn`), so what was decided
  for one prompt never goes into the next. A screen read before the answer
  (a reader a step behind the radar) brings no card back. A dialog still up
  that the tower can no longer read is not "answered": it closes
  `Unread`, its decision `stopped`.
- **A question at the end of a turn** (`question-text`, a wait card): the
  user's reply typed there comes with the session's `UserPromptSubmit`
  hook. The set and its decision close as answered, with the user's words
  ("… -> Answered in Shop › api: Wait, I'll review it first"), whether the
  hook or the radar (the session working again) is first.
- **A prompt that isn't the user's leaves it open.** The session working
  again closes such a question only when what it works on is the user's.
  A prompt the tower typed (an instruction, a message from another
  session, an introduction; one typed in while it worked and sent when that
  turn ended), another terminal's, or the session's own (Claude Code's
  `<task-notification>` that its background work finished) leaves the
  question open, and its decision with it (`Asks::went_on`). The tower
  knows its own typing as it types (`typed_lately`, the send queue's
  `typed_in_while_working`); for the rest the hook says, and when the radar
  is first the question waits up to six seconds for it: the user's words
  close it with them, any other prompt keeps it, and no word at all (an
  agent with no hook) closes it as answered in the terminal, as before. A
  question answered on its card closes when its reply goes in, as before.
  What then: the session works on that prompt and the question stays the
  user's (on the Mac, the phone, `waiting`). If that turn ends asking
  something again, the new question takes the old one's place (one
  question at the end of a turn per session: the old card and its decision
  close, as before). If it ends asking nothing, the question stays open
  until the user answers it (on its card, the reply going in as the
  session's next prompt; or in the terminal), dismisses it, or the session
  ends: the session's own next turn is not taken for an answer. A reply the
  tower held for that session (answered on the card while it was busy) is
  not typed: the record has a `queue` entry with `status: not-sent`, "Not
  sent: you answered it in the terminal (the reply was "…")". Never said.
- **Whose a prompt is** (`daemon::typed`): the tower writes down every
  text it types into a session (the send queue: instructions, messages
  between sessions, introductions; a reply to a question) before typing
  it. A prompt is the tower's when its words are those of a text it typed
  into that terminal in the last ten minutes that no prompt took yet, or
  when it comes while the tower types there or within five seconds after
  (words a session reports differently: wrapped as pasted, a file as
  `[Image #1]`). The task a session was started with (the last word of its
  command line), an introduction, and a prompt `hc send` brought from
  another terminal (`<hc from="…">`: a brain, another session) are not the
  user's either. Anything else is the user's, typed in that terminal: the
  same words as something the tower sent long ago too.
- **New work typed there** (the user's prompt, no question open): their
  request in that session. A history entry of kind `request` with `source:
  terminal:<address>`, `session`, `name`, the session's `space`, `agenda`
  and `record: true` (in the Record as "You typed", in that space's record,
  never a chat message, never spoken), and an agenda of origin `terminal`
  with one task, running from the start (`Ledger::typed`). More typed in
  that terminal while it is unfinished is more of the same task; what the
  tower hands that session is never part of it. "What's left?" and the
  space's mission requests count it. Its turn's end settles it (verified,
  or failed when it says it couldn't; the user saw it end there). No brain
  hears of it: no `[agenda]` note, no retry or decision when it fails, no
  receipt owed, no watch. A question at the end of such a turn is the
  user's card at once (`TYPED_THERE`: no note to the brain, nothing said),
  and so is a wait.
- **What the tower had queued** for that session stays queued and goes in
  when the session is free, as before; only a reply to a question the user
  just answered there is dropped (above).
- **A blocked session the tower couldn't read** (`blocked`) closes as
  answered "in its own terminal" once it is no longer blocked, as before;
  its record entry now has `source: terminal:<address>`.

### Idle is not blocked

A turn that ends saying only that the session is ready (it waits for
instructions, has no task yet, stands by: `questions::ready_only`), from a
session nothing was asked of, is `ready`. It makes no question set, no
alert and no decision; a record entry says "<name> is ready".

- "Nothing was asked of it" (`asks::has_work`): no instruction waits for it
  in the queue, and it is on no agenda task (one queued, running or blocked,
  or one whose state changed in the last minute), or that task itself told
  it to wait for instructions.
- A session that was asked something and ends that way still makes its
  "waits for you" card, as before.
- A sentence that names something for the user to do (merge, review, test,
  decide, a PR or issue number, "blocked", "before I …") is never ready. A
  question, a request, or word that something is wrong (blocked, failed,
  missing, can't) in the last four sentences makes the whole message a real
  wait.
- The agenda's judge takes "waiting for your instructions" as done, not as
  a question, when that is what the task asked for.
- "Waits for you" cards an older tower made for such turns are dropped at
  the next start.

### Order, and who waits

- `waiting` counts the sessions blocked on it now: a session waits unless
  it is `working` on something else (the radar says). A question's
  sessions wait for as long as their set is open.
- **Order.** `weight = sessions waiting × time since first_ms`, heaviest
  first, then oldest first, then by id. The time is in milliseconds, so two
  decisions in their first minute are still ordered. The daemon publishes
  the order again every 30 seconds.

### Mute

A muted space's decisions are left out of the main queue and the orb's
count, are not said, and make no alert. They are returned when that space
is asked for, and `state.json` carries them marked `muted: true`.

### Telling the user once

- When a decision first appears: one alert (a chat item) and one spoken
  sentence, through the alert a blocked session makes (held by the focus,
  silent while the tower is muted). A question: "<name> asks: <what>. I'd
  say: <pick>." A wait: "<name> waits for you: <request>". A merge: "<name>
  opened PR #N into <base>: it waits for your merge, in the chat". A raise:
  "<name> needs you: <text>. I'd say: <pick>". A failed task: "<name> needs
  you: "<task>" failed after a retry: <why>. Try it again, or drop it?".
- The same key again says nothing and adds no chat item, at `decisions`
  and `results`. At `everything` the alerts the tower always made stay
  (one per question set, per raise, per turn that ends on a PR), and a
  failed task is told by the brain as before.
- A watched session that ends its turn waiting for the user: at the quiet
  levels the tower says it itself, the plain "<name> is waiting for you"
  beside it is only written down, and the brain's `[watch]` note says the
  tower has put it to the user and not to raise it again.
- A merge decision made beside a wait for something else (a session asks
  for a review of its PR) is in the queue without a sentence of its own.
- What is open when the daemon starts was told by the run before: nothing
  is said again for it.
- Not a decision: a session blocked on a dialog the tower cannot read
  (another machine's, an unknown menu). It still gets the plain notice each
  time it blocks.

### Commands and requests

- `tower decisions [--space <s>] [--all] [--json]`: the queue in order.
  JSON when piped or with `--json`: `{"space": name or null, "count": n,
  "decisions": [Decision], "closed": [Decision]}` (`closed` with `--all`,
  newest first). No space: the main queue, muted spaces left out. `--space`
  (a name, or a part of it): that space's own, muted or not.
- `tower answer <id> --choice <n>` (an option's index, from 0) or
  `--other "<words>"`; `--json` takes `{"choice": n}`, `{"other": "…"}`, or
  for a question the answers a question set takes. What it does, by kind:
  - `question`, `permission`: answers the question set of every session of
    the decision (`asks.answer`, as `tower answer <question id>` does).
    `--other` works when the set has one question. Answer:
    `{"ok":true,"id","state":"answering"|"answered","answered":[{"session","question","status"}],"errors":[…]}`.
  - `raised`, `choice`: the chosen option's `text` (or "About "<title>":
    the user decided: <label>.") goes to its waiting sessions through the
    queue as an instruction (to all of them when none waits), and the
    decision closes. Answer: `{"ok":true,"id","state":"answered","answer","sent":[{"session","name","queue_id","sent"}],"errors":[…]}`.
  - `failed`: `0` sends the task again (`tower agenda requeue`), `1` drops
    it (`abandon`), `--other` sends it again with those words.
  - `merge`: refused, naming its PRs. A merge is the PR's own flow (the
    panel's chip, the phone's `/v1/prs/{key}/confirm` then `/merge`), by
    `prs[].key`. Nothing merges through the socket.
- `tower dismiss <id>`: out of the queue, nothing sent (a question's sets
  are dismissed with it).
- `tower hold <id> [--reason "<the user's words>"]` and `tower unhold <id>
  [--why "<what is done now>"]`: see "On hold" below.
- `tower answer` and `tower dismiss` still take a question set's id as
  before; `tower questions` is unchanged.
- `tower space mute|unmute <space>`.
- The socket: `{"cmd":"decisions","space":null,"all":false}`; `answer`
  (`target` a decision id, `answers` the body above) and `dismiss` (`id`);
  `{"cmd":"hold","id","reason","unhold":false,"by":"mac"}` (`by` none: the
  caller's terminal says, a brain's name, else `cli`).

### On hold

"Wait for the ingest filters, then we go for prod", said about a merge
card, means the card is still the user's but not now. Holding it
(`decisions::Hold`, in `src/decisions.rs`; the daemon's part in
`src/daemon/decisions.rs`):

- **How**: the card's **Hold** (on every card's head, beside its ×) opens a
  field for what it waits for (optional; Enter holds it); `tower hold <id>
  --reason "…"`; the phone's `POST /v1/decisions/{id}/hold`; or a tower,
  told so in chat: its instructions say to hold, never dismiss, with the
  user's words (`assets/tower-core.md`, "On hold, not dismissed").
- **Held**: still open, still listed, but out of the queue and every count
  (the state has it in `decisions_held`, not `decisions`; not in
  `decisions_count`, the orb's badge, the phone's `count`, a mission's
  `needs`), and never told (`Decisions::tells` says no and doesn't mark it
  told). The panel lists held ones under "On hold · n · not counted" below
  the queue (All spaces) and under "Needs you" (a space), each with the
  user's words, who held it, since when, what brings it back, and "Back to
  waiting". `tower decisions` lists them apart.
- **Back** (`Book::wake`, after every change; `Book::unhold`): it is in the
  queue again, counted, and told once (`came_back`: the alert "Back from
  hold: <title>. <why>.", said as a raise is), with `back.why` on its card:
  - updated since it was held (`Decision::update_since` against
    `Hold::seen`): a PR's head commit (`PrRef::sha`) or checks changed, a PR
    joined it, it came up again (`count`: raised again, another session
    asked), what it asks changed, more detail. A review or score moving is
    not an update;
  - its session posted about it: a session in its space ends its turn with
    a message naming a held merge's PR (`Decisions::posted`);
  - a PR its reason names merged (`Hold::until_prs`, from `#829`, `PR 829`
    or a PR's URL in the reason; its own PRs aside);
  - a PR merged in its space since the hold, read once against the
    reason by the tower's model (`judge_merged`, one headless call as
    `covered` makes; `Hold::weighed` keeps each read once) and found to
    be what it waited for;
  - let go: `tower unhold`, "Back to waiting", `/unhold`, or a tower that
    saw what it waited for done.
- History: a `decision` entry with `status` `on hold` or `back`, for the
  record.

### In `state.json`

- `decisions`: every open decision in queue order, in the JSON shape under
  step 11, each with `muted`. The main queue is those with `muted: false`.
  (The phone's state leaves the list out and keeps the count: step 11.)
- `decisions_count`: how many of them are outside muted spaces (the orb's
  number). None on hold.
- `decisions_held`: the open decisions on hold, as `decisions` has the
  others, newest hold first (left out of the phone's state, as `decisions`
  is: the phone has them in `GET /v1/decisions`' `on_hold` and the
  `decisions` event).
- `questions` is unchanged.

`decisions.json`: `{"open": [Decision], "closed": [Decision], "settled_prs":
{"owner/repo#n": "dismissed"|"stopped"}}`, each decision as stored (the
shape below plus `told`, `tabs`, `left`, `answer`, `closed_ms`, and each
option's `answers` or `text`).

## A command gets a receipt: acts, and undo

Asked to open a session or a space, the tower takes the defaults, acts, and
keeps an **act**: what it opened, what it assumed and why, and the request
it answered (F5, F6, W11, A8). The model is `src/acts.rs`; the daemon keeps
them in `src/daemon/acts.rs` and in `acts.json` (the state folder, the last
50) across restarts.

```
Act { "id": "r" + 12 hex digits, "kind": "start" | "launch",
      "at_ms", "by": "tower" | "tower-<slug>" | "phone:<device>" | "cli" | "<a session's address>",
      "space": name or null,
      "line": "Opened notes fix in sandbox.",       // the reply, in one line
      "sessions": [ { "address", "name", "label", "worktree": path or null, "branch": name or null } ],
      "assumed": [ { "what": "account" | "model" | "folder" | "worktree" | "space" | "name" | "setup" | "worktrees",
                     "value": "orca", "why": "what \"second\" names" } ],
      "request": { "at": the request's history `at`, "text" } or null,
      "state": "done" | "undone",
      "undo": { "possible": bool, "why": text or null },
      "closing": "Closed it. Nothing else changed." or null, "undone_ms": n or null }
```

- **Who makes one.** `tower start` after it opened its session (unless
  `--no-act`, which `tower launch` passes: a launch keeps one act for its
  whole space, with its sessions that opened), so a `tower dispatch` that
  starts sessions makes one through its starts. The command sends the
  daemon `{"cmd":"act","action":"record","act":{kind, space, space_made,
  goal, sessions:[{address,name,worktree:{repo,path,branch}|null,fresh,first,task}],
  assumed}, "from": <its HC_TERMINAL_ID>}`.
- **What was assumed.** `tower start` prints `assumed` as `tower launch`
  does, each thing with its why: `{"account","account_why","model",
  "model_why","folder","folder_why","worktree","worktree_why","space",
  "space_why","name","name_why"}` (`account_why`: "as asked", "what
  \"second\" names", "the default one: none was named"; `model`: "default
  model", since a session starts on its account's own; `folder_why`: "as
  asked", "the best match for \"notes\"", "the folder the command ran in";
  `space_why`: "as asked", "made: no space had that name", "the one the
  window shows"), and `act` (the act's id), and `worktree_fresh`. The act's
  `assumed` is the same as a list of chips (a folder and a worktree by
  their last part).
- **Whose request.** Run by a brain (`from` is the main brain's terminal or
  a space's own) on a turn the user began (not a note turn), the act
  answers the latest request asked of that brain in the last half hour
  (`acts::request_for`): `request` names it. Sessions opened one by one for
  the same request are **one act** (a second `start` joins the first: one
  line, "Opened 2 sessions in M5 Pro: api and web.", one Undo). Run on a
  note turn, from a shell, the full window, a phone or the tower's own
  queue, it answers none: it is kept and can be undone by its id.
- **History.** Kind `act`: `{"kind":"act","id":"r…","status":"done",
  "text": the line,"space","act":{kind,by,assumed,sessions,request_at,
  request_space}}`, with `record: true` when it answered no request. Said
  again when a session joins it, and once more when it is undone
  (`"status":"undone"`, `text` the closing line). `chat::shown` takes one
  that answered a request: it is that request's receipt in the chat (one
  message, `kind: act`, changed in place).
- **Published.** `state.json` has `acts`, the latest 10, newest first, as
  above; the phones get an `act` event at every change, and the chat
  message carries it (step 11).
- **Undo.** `tower undo [<id>] [--force] [--json]` (no id: the latest act,
  when it was made in the last 10 minutes and isn't undone); `tower acts
  [--json]` lists them (`{"acts":[Act]}`, newest first). The socket:
  `{"cmd":"act","action":"undo","id":…,"force":bool}` and
  `{"cmd":"act","action":"list"}`. Undoing (`acts::undo`):
  - the space the act made (`space_made`: a launch that made its space, a
    `tower start` that made one), with no agent session in it but the
    act's own, is closed as `tower space close --force` closes one (its
    brain, its requests, its terminals, its clean worktrees);
  - otherwise each of its sessions that is still live is ended (`hc kill`,
    `hc rm`) after its agenda task is dropped, what waits for it in the
    queue is cancelled and its watch is forgotten (so nothing is told as
    stalled or failed), and each worktree the act made (`fresh`) is removed
    when nothing in it would be lost and no other live session works in it.
    A worktree that was there before is left there;
  - it is **refused**, naming the session and why, when a session was given
    more work than its first instruction (a prompt its hooks report, or an
    instruction queued to it, that is not its first instruction, the
    tower's introduction or another session's message: `acts::is_work`), or
    when its worktree holds changes that are nowhere else
    (`worktrees::dirty`). `--force` undoes it anyway: the sessions are
    ended, and a worktree with changes is kept and named;
  - it answers `{"act": Act, "closing", "sessions_ended": [address],
    "worktrees_removed": [path], "kept_worktrees": [{"path","why"}],
    "space_closed": bool}`, and the act is `undone` with its `closing`
    ("Closed it. Nothing else changed.", or "Closed it. Kept its worktree:
    … (1 uncommitted change (notes.txt)).").
- **The reply.** Both windows show the act with the reply to its request
  (`home::last_reply_with`): its line, what was assumed as chips (why, on
  hover), the tower's own words under it, and **Undo** (`tower undo <id>`).
  Where work would be lost the block says why and offers "Undo anyway"
  (`--force`). Undone, it shows the closing line and nothing else.
- **The brain** is told (in `assets/tower-core.md`) to act with the
  defaults, say in one short line what was opened, never ask which
  account, model, folder, space or name, and to run `tower undo` on the
  user's word ("undo that").

## Make it a rule

After a card is answered on the Mac, its receipt line stays about 8 seconds
(`model::RULE_MS`), or until the next card is answered, and carries "Make
it a rule" (`home::make_rule`). It asks the tower of that decision's space,
as a typed request (`tower ask [--space <space>] …`, so a space's own tower
when it has one):

```
Make this a rule: when a session in <space> asks "<the decision's title>", answer "<the answer given>" without asking me.
```

- The wording and when it is offered are one function for the Mac and the
  phone (`decisions::rule_for`): only for a `question` or a `choice`
  answered in words (an option's label, or the user's own; a question set
  with several questions offers none), never for a `merge`, a
  `permission`, a `failed` task or a plain `raised`, and never when the
  title, the reason or the answer is about what the user always decides
  (`decisions::always_yours`: a deploy or release, a delete or overwrite,
  production, secrets, money).
- The tower's existing behavior flow does the rest: it adds a behavior
  (`tower behavior add`, category "Questions 90/10"), which shows in the
  chat with what changed and can be undone there. The core prompt tells it
  to write the rule narrowly (that space, that kind of question, that
  answer) and never for the always-ask list; the 90/10 behavior tells it to
  answer such a question with that rule from then on.
- `tower answer <decision>` (and the phone's `/answer`) says `"rule":
  {"can": bool, "text": the request or null}`.

## Names you can read

A session is its space and what it works on: `sandbox › mediaroom` where it
is written, "the mediaroom session in sandbox" where it is said
(`src/names.rs`, the one place).

- What it works on is its own name when it has one, without its space's
  words in front (`fix-login-eyzflow` in the space "fix-login" is
  `eyzflow`), else its project: the repo of the worktree the tower gave it
  (`tower.repo`), else the folder it runs in, without the name of the
  folder it is in (`eyz/eyz-mediaroom-serverless` is `mediaroom
  serverless`), else its address. This short name is what `hc::spoken_name`
  returns, so it is what a decision's `sessions[].name`, a mission's
  `sessions[].name`, a thread's `from_name` and `to_name`, `agents[].name`
  and the history's `name` hold: each has its space beside it.
- `names::shown(space, name)` and `names::said(space, name)` put the two
  together, and read a name an older tower kept ("Claude in eyz mediaroom
  serverless") the new way. A space's own brain is `sandbox › tower` and
  "the sandbox tower"; the main one "the tower".
- A notice about a session ("mediaroom asks: …", "… waits for you", "…
  needs you", "… finished", "… hit an error") is written with the first and
  spoken with the second (`names::notice`, applied where the radar shows
  and says it): the chat has `sandbox › mediaroom asks: Which database?`
  and the voice says "The mediaroom session in sandbox asks: Which
  database?".

## Step 5 and 8: a space is a mission with an end

The model is `src/missions.rs`; the daemon runs it in
`src/daemon/missions.rs` (every 20 seconds, and at once when a session
starts, stops or ends, or a `tower space` command changed something).

### In `spaces.json`

A space's goal is its agenda, where it always was (`agendas`, by space
name; the orb's panel reads and writes it there). The rest of the mission
is under `missions`, by space name:

```
"agendas":  { "<space>": "<goal>" }
"missions": { "<space>": {
    "done_when": "…",          // left out while empty
    "standing": true,           // left out while false
    "state": "open" | "quiet" | "ready" | "closed",
    "report": "…", "report_ms": n,   // the closing report; left out while there is none
    "report_asked_ms": n,       // when the brain was asked for it
    "born_ms": n,               // when the tower first knew the space
    "goal_ms": n,               // when its goal was last set
    "seen_ms": n,               // when a live session was first seen there
    "quiet_since_ms": n, "ready_since_ms": n, "idle_since_ms": n,
    "closed_ms": n, "archived_ms": n,
    "worktrees": [ { "repo": "…", "path": "…", "branch": "…" } ],   // made by the tower for it
    "setup": "name",            // the setup `tower launch` made it with
    "review": false,            // its PRs get no reviewer ("PR review"); left out while on
    "goals": [                  // how its goal came to be, oldest first; left out while empty
      { "goal": "…", "done_when": "…", "at_ms": n,
        "by": "launch" | "you" | "request" | "tower" | "",
        "request": "…", "why": "…" }   // for `request`: the user's words, and what it added
    ]
} }
```

### A goal follows the user's requests

`src/goals.rs`, run by `src/daemon/goals.rs`. A space's goal is made for
what it was first for; the user asks for more while working there, and the
goal follows:

- **Heard**: what the user asks about a space (of its own tower, or of the
  main one: `daemon::brains::ask`, once a brain took it), and what the tower
  hands out into a space for a request of theirs (`tower dispatch`, `queue`,
  `start`: an agenda of origin `user` whose task is in that space,
  `daemon::agendas::register`). The same words in the same space are
  weighed once (6 hours); a bare reply or two words are not
  (`goals::worth_weighing`). One heard before the goal was last set other
  than by a request (the launch's own ticket, a goal set by hand after it)
  is in the goal already.
- **Which spaces**: one with a goal, not standing, not closed.
- **Weighed** one at a time on a thread of the daemon's, by one headless
  call (as `covered` does it: the default account, `[brains] model` at low
  effort, no tools, nothing kept, 60 s at most) with the space, its goal,
  done-when, how the goal came to be and the request (`goals::prompt`). It
  answers `{"changes": false, "why"}` for a step toward the goal (a fix, a
  test, a review comment, a question, go on, commit; unsure: no change) or
  `{"changes": true, "goal", "done_when", "why"}`: one short line that keeps
  what the space was first for and adds the new scope, and the conditions
  as they were plus the new ones. Checked: one line of at most 200
  characters, a done-when of at most 600; the same goal is no change.
- **Applied** (`missions::goal_from_request`) only when the goal and
  done-when are still the ones weighed: an edit by hand meanwhile wins. The
  version goes into `goals` with `by: "request"`, the request and why;
  `goal_ms` moves (only what is finished after it makes the space ready); a
  ready space is open again and its report, of the goal before, goes. The
  space's own tower's instructions say the goal and the request that
  changed it.
- **By hand**: the Goal card's Save runs `tower space goal … --by you`; the
  phone's `POST /v1/spaces/{space}/goal` is the user's too. A launch is the
  first version (`by: "launch"`); `tower space goal` without `--by` is the
  brain's (`by: "tower"`). Clearing a goal clears its history. A goal set
  before the tower kept versions goes in first (`by: ""`) when it changes.
- `tower space goal-request <space> "<words>" [--dry-run]` weighs words as
  such a request by hand.
- **The Goal card** (a space's view): "Goal · updated by your request at
  3:55 PM" (or set by you, set at launch), the goal, "Your request: “…”
  (what it added)", what counts as done, and **N earlier versions**,
  folded, newest first: each its time, who set it, the goal, its done-when
  and the request. `HERDER_TOWER_ORB_GOALS=open` (looking at it with test
  data) shows every card's earlier versions.

`follow` and `mute` stay the spaces' own lists. Fields that don't apply
are left out. A spaces file from before missions loads as it is. Every
change goes through `spaces::update` (one writer at a time, under a file
lock): the missions, a goal, follow and mute, the focus and the digest's
read marks. `tower space follow|unfollow|mute|unmute` also has the daemon
look at the missions at once, so `missions[].follow` and `.mute` in the
state say it without waiting for the next look.

### States

- **open**: it has live sessions, or the tower learnt of it in the last 10
  minutes (a space just launched, a goal set ahead of its sessions).
- **quiet**: no live session, from the moment the last one goes. Its held
  news is marked read (`read_ms`: its unread counts are 0), what was raised
  there and its failed tasks close, its merges stay (step 3), and a record
  entry says so. A session there again makes it open. After 24 hours quiet
  it is **archived** (`archived_ms`): kept in the file, left out of `tower
  spaces`, `tower missions` and `state.json`; its merges close then.
- **ready**: it has a goal, isn't standing, has at least one request
  (agenda) finished since the mission began and its goal was set, no
  unfinished one, and no open decision. The daemon
  then asks the brain once for a closing report, in a note: `[space]
  "<name>" looks finished: … Write its closing report now …: tower space
  report "<name>" "<the report>"`. `[space]` is a note tag
  (`brain::is_note_prompt`), so that turn is a note turn; at `everything`
  the brain also says in one line that the space is ready to close. A new
  request or a new decision there (or `standing on`, or the goal cleared)
  takes it back to open, and its report is dropped.
- **closed**: by `tower space close`, or by itself (below). Closed spaces
  stay closed whatever is seen there; `tower space goal` with a goal, or a
  session opened there by `tower start` / `tower launch`, starts the
  mission over as open.
- A **standing** space is one the user keeps: never ready, never closed by
  the tower, and its sessions get no worktrees (step 10). It still goes
  quiet and is archived like any other.
- A space with **no goal** is not a mission yet: it is never ready, so the
  tower never closes it, and its sessions get no worktrees (step 10). It
  is listed with the others (`goal: ""`), goes quiet and is archived.

A "live session" is an agent session (Claude Code, Codex) in the space:
not a plain program, not a brain (`tower`, `tower-…`), not `tower-voice`.
A space's sessions are read from Herdr's core (`hc ls`, each one's labels
from `hc get`, kept 30 seconds), here and on every machine. When Herdr, or
a machine, can't be asked, nothing moves that round.

### Commands

- `tower space goal <space> "<goal>" [--done-when "<what counts as done>"]`
  ("" clears the goal; `tower space agenda` is the same command). Prints
  `{"space","goal","done_when","standing","state","report"}` when piped.
- `tower space standing <space> on|off`. A name no space has yet is kept
  for the space that will have it (`on` only), so a space can be standing
  before its first session.
- `tower space report <space> "<text>"`: the closing report (the brain's
  answer to the `[space]` note). Kept with the mission and shown with it.
- `tower space review <space> on|off`: whether the PRs its sessions open
  get a reviewer ("PR review"). On unless turned off; a mission from before
  the switch reads as on. Off ends a reviewer that is open there (the
  daemon looks at once). A name no space has yet is kept for the space that
  will have it. Prints `{"space","goal","done_when","standing","review","state","report"}` when piped.
- `tower space close <space> [--force]` (the user's word only):
  1. refuses while an agent session there is `working` or `blocked`, naming
     them, unless `--force` (a brain doesn't count: it is the one closing);
  2. closes the open requests (agendas) that are about that space only
     (`tower agenda close --force`: their unfinished tasks are abandoned);
  3. ends the space's own brain (step 7), then the space's terminals.
     Herdr closes a whole space in one command
     (`herdr-app do space.close '{"id":…,"confirmed":true}'`, which ends
     every terminal in it and removes the space). A space that holds the
     tower's own terminals (`tower`, `tower-voice`) is never closed whole:
     its other terminals are ended one by one (`hc kill`, then `hc rm`) and
     Herdr's space stays. The same one by one when Herdr refuses, or when
     its window doesn't show the space (then by the terminals' `where`);
  4. removes the worktrees the tower made for it, each only when clean: no
     uncommitted change (the copied `.env*` files aside) and no commit on
     its branch that is on no remote and no other branch. A dirty one is
     kept, stays in `missions.<space>.worktrees`, and is named with why.
     Branches are never deleted. If a terminal couldn't be ended, every
     worktree is kept;
  5. writes a record entry (`history.jsonl`: `kind: "space"`, `status:
     "closed"`, `space`, `text: "<name> is closed. <report>"`, `record:
     true`). The same kind records `quiet` and `ready`;
  6. sets `state: closed`, `closed_ms`, `archived_ms`, marks its held news
     read, and ends a focus on it. The daemon then closes its decisions,
     merges too.

  It prints `{"closed": name, "sessions_ended": [{"address","name"}],
  "sessions_left": [{"address","name","why"}], "herdr_space": "closed" |
  "kept" | "not shown", "worktrees_removed": [path], "kept_worktrees":
  [{"path","branch","repo","why"}], "requests_closed": [agenda id],
  "tower_ended": [its own brain's name, when it had one], "report": "…"}`.
  It works with no daemon running.
- `tower space resolve <space> [--force]` (the user's word only, a space
  with a goal): its goal is achieved. The same close, refused the same way,
  but the mission keeps `achieved_ms`, the record's line says `"<name> is
  resolved: its goal is achieved (<goal>), and it is closed. <report>"`, and
  the receipt has `"resolved": true` and `"goal"`. A space with no goal is
  refused (close it instead). `--dry-run` changes nothing and prints what
  it would do (`missions::preview`): `{"space", "goal", "done_when",
  "standing", "report", "ends": [{"address","name","status","agent",
  "busy"}], "stays": [{"address","name"}], "busy": [...], "worktrees":
  [{"path","branch","repo","removed","why"?}], "herdr_space"}`: `ends` is
  every terminal it ends, agent sessions and plain shells alike; `stays`,
  the tower's own; a worktree with work not committed or pushed is kept
  (`removed` false, and why).
- **Resolve, in the full window.** The head of a space with a goal (not a
  standing one) has "✓ Resolve". It asks first, in a card above the
  space's view: what resolving does, and a line for each terminal it ends
  ("Ends clips (t9): a session at work.", "…: an open terminal."), each
  that stays, each worktree it removes or keeps (and why), and whether
  Herdr's space goes. A session at work or waiting for the user is named
  ("clips (t9) is still working. Closing now cuts that work off."), and the
  button is "Close anyway" (`--force`); otherwise "Resolve and close".
  Cancel puts it away. Once done the window goes to All spaces, saying what
  was ended, removed and kept. Its own tower (`tower-<space>`) is named in
  the card too ("Ends its own tower, tower-isa (t13)."): a close ends it
  first. `HERDER_TOWER_ORB_RESOLVE=<space>[:go]` presses Resolve (and with
  `:go` its button) a moment after the start, for looking at it without the
  pointer. A refusal (something started there since
  the card was read) shows in the card, with "Close anyway".
- **Auto-close.** A ready space whose sessions have all been `idle` or
  `done` for `[spaces] auto_close_hours` (0, the default, is never; say 24 for a day), without a break,
  is closed the same way, without `--force` (`idle_since_ms` is when that
  wait began). So is a space that was ready when its last session left,
  after its day quiet. A close that fails is tried again after the same
  hours (a quiet one is archived instead).
- `tower spaces [--all]`: each row gains `goal` (the same as `agenda`),
  `done_when`, `standing`, `state`, `report`, `quiet_since_ms`,
  `ready_since_ms`, `closed_ms`, `archived`. Archived spaces (quiet for a
  day, or closed) are left out unless `--all`.
- `tower missions [--json]`: what `state.json` has under `missions` (below).
  The daemon looks at the sessions again first; with no daemon it is built
  from Herdr and the tower's files (then without `said`).

```toml
[spaces]
worktrees = true          # step 10
port_range = "3000-3050"  # step 10
auto_close_hours = 0      # the default: a ready space waits for the user; 24 closes it after a day idle
```

### `missions` in `state.json`

Also the answer to `{"cmd":"missions"}` on the socket and to `tower
missions --json`:

```
"missions": { "spaces": [Mission], "quiet": ["name"], "closed_today": ["name"] }

Mission { "name", "goal": "" when none, "done_when": "" when none,
          "standing": bool, "review": bool,   // its PRs get a reviewer ("PR review")
          "state": "open" | "ready" | "quiet",
          "follow": bool, "mute": bool,
          "needs": n,            // open decisions in that space, muted or not
          "report": "" when none,
          "sessions": [ { "address", "name", "status",
                          "account": name or null,
                          "worktree": path or null, "branch": name or null, "port": n or null,
                          "said": its last words (cut to 240 characters) or null } ],
          "requests": [ { "id", "title", "state": "open"|"done"|"closed"|"abandoned",
                          "done": n, "total": n } ],
          "thread_count": n,
          "tower": { "name", "status": "ready"|"working"|"starting"|"blocked"|"off",
                     "address": address or null } or null,   // its own brain (step 7)
          "quiet_since_ms": n or null, "ready_since_ms": n or null,
          "auto_close_ms": n or null }
```

- `spaces`: every mission that is not closed and not archived, in a stable
  order: the open and ready ones first (those with `needs` > 0, then those
  with a session `working`, then the rest; then by name, any case), then
  the quiet ones (by name).
- `quiet`: the names of the quiet ones in `spaces` (the fold).
- `closed_today`: spaces closed since local midnight, oldest first.
- A session's `name` is its name within its space, as `agents[].name` has
  it (`fix the eyzflow` for a session named `fix-the-eyzflow`, `eyzflow`
  for `fix-login-eyzflow` in the space "fix-login", its project when it
  has no name: see "Names you can read"); `status` is
  Herdr's word (`idle`, `working`, `blocked`, `done`, `error`). `said`
  comes from the session's hooks, so it is null until its first turn ends.
- `requests`: the space's open agendas (oldest first), then those finished
  since local midnight (newest first). `done` counts tasks that are
  verified or dropped; `title` is cut to 80 characters. Only what the user
  asked for (`Agenda::request`, a view: the ledger is kept as it is):
  - A task that is a message one session sent another is no part of a
    request: by its recorded sender (`from` on the task, while the tower
    tells everything), or, in what an older tower kept, by how it opens
    ("From t7h: …", "From web (t7h): …", "From Shop › web (t7h): …":
    `peers::sender_in`). An agenda of nothing else is no request at all,
    and neither is what an older tower took for the user's typing that was
    such a message or a session's `<task-notification>` (the prompt hook now
    tells both apart: `daemon::typed`). `done` and `total` count the
    request's own tasks, and it is done once they are (finished at the last
    of them), whatever messages were left queued in it.
  - A request belongs to one space and is listed there only: the one the
    user asked it in (the space it was opened with), else that of its first
    task of its own. A space only a message between sessions took it to is
    not its own. The same counts make a space `ready`.
- `thread_count`: the messages in the space's thread as the state carries
  it (at most 50).
- `auto_close_ms`: when a ready space closes by itself (null: not ready,
  a session isn't idle, or `auto_close_hours = 0`).
- The phone's `state` (the call and the event) leaves `missions` out, as it
  does `threads` and `decisions`: `GET /v1/missions` (step 11) serves it
  (the daemon's own look, as `{"cmd":"missions"}` answers it), and a change
  is a `missions` event.

### After a restart: what was open comes back

The tower keeps what is open, so that a restart of the Mac (or of Herdr's
core) doesn't lose it: `open.json` in the state folder (`crate::reopen`,
`daemon::reopen`).

- **Kept**: with every look of the missions (every 20 s), each live agent
  session of a space: its space and tab, its folder, its account, its
  program (`claude-code`, `codex`), its conversation (the session id its
  hooks gave, on its terminal as `tower.session`), the flags of its command
  line a resume keeps (`--model`, `--effort`, `--permission-mode`,
  `--add-dir`, `--settings`, `--dangerously-skip-permissions`; never its
  task), and the tower's `tower.*` labels on its terminal (its worktree,
  branch, port, name). Not the tower's own brains, planner and reviewers
  (the tower starts those itself), nor another machine's sessions.
- **Lost or ended.** Herdr's core says when it started (`hc server status`,
  its uptime). A kept session missing from a look, last seen under an
  older core, was lost to the restart: it is kept to be offered back, for a
  week. Missing under the same core, it ended while everything stayed up
  (the user quit it, the space was closed or resolved): it is forgotten
  once it has been gone 3 minutes. A shutdown takes the daemon down well
  before that, so what it sees ending on the way down stays lost, not ended.
- **The first look** of a tower that has no `open.json` yet takes, as lost,
  the sessions its hooks told of before this core started whose terminal
  Herdr kept as a shell at its prompt, still labelled with that
  conversation, in a space that isn't closed or archived.
- **The offer**: once Herdr's window answers (`herdr-app ls`), one card in
  the main queue (see the table above). It is raised once per start of the
  core, changes in place as sessions come back by hand, and stops when none
  is left. Dismissed, it isn't raised again until the next restart; `tower
  reopen` still lists what is waiting. When the tower's own brain isn't
  running either, the card says so, and reopening starts it again (a
  space's own brain waits for the main one).
- **Putting it back**: each session resumes its own conversation where it
  was: `cca <account> -n <name> <flags> --resume <id>` (its account when it
  was known, else the default one; plain `claude` without accounts), or
  `codex resume <id>`.
  - Its own terminal, when Herdr kept it as a shell at its prompt in the
    same space and still tied to that conversation: `cd <folder> && <the
    command>` is typed into it.
  - Else a new tab of its space (`tab.new`, its tab's name, its folder),
    with its `tower.*` labels set again.
  - Else its space is made again (`space.new`, the folder Herdr had for
    it; for a session in a worktree, on a home tab first, as `tower
    launch` makes one), and the space's next sessions are its tabs.
  - Not when its folder is gone (a removed worktree) or its conversation's
    file is: that one is said, the rest go on. One the tower put back that
    isn't seen again within 2 minutes (its resume failed) is lost again.
  - The window is left on the space it showed. Each space gets one line in
    the record (kind `space`, status `reopened`).

```
tower reopen                      # what a restart closed and is waiting
tower reopen go [<space>…]        # put them back (all, or those spaces')
tower reopen go --dry-run         # how each one would go back
tower reopen forget [<space>…]    # let them go
```

`{"cmd":"reopen","action":"list|go|forget","spaces":[…],"dry_run":false}`
on the socket. A test daemon (`HERDER_TOWER_TEST=1`) keeps only its test
sessions, starts no brain, and may be told another start of the core
(`HERDER_TOWER_CORE_SINCE=<file>` holding Unix ms), to try a restart
without making one.

## Step 9: one sentence launches a space

The code is `src/launch.rs` (the plan, the launch), `src/ticket.rs` (the
draft of a ticket), `src/plans.rs` and `src/daemon/plans.rs` (a draft's
plan, its planner and the chat about it), `src/setups.rs` (the projects)
and `src/orb/panel/launch.rs` / `projects.rs` (New space, Projects).

### Projects

A project is a `[[setups]]` table (the name stays, for the files and the
phones from before): a name, the account its sessions run on, a line on
what it is, its repos, and a line on what each repo is in it.

```toml
[[setups]]
name = "EYZFlow"
account = "orca"
about = "the video platform agencies run their media and customer support on"
repos = ["~/Work/eyz/eyzflow", "~/Work/eyz/eyz-mediaroom-serverless"]
roles = { "~/Work/eyz/eyzflow" = "the web app and its API", "~/Work/eyz/eyz-mediaroom-serverless" = "the media backend" }
```

`about` and `roles` are optional: a table from before reads as one without
them, and is written back without them. A role is keyed by the repo as
`repos` writes it (`~` and a trailing `/` aside, `Setup::role_of`).

- `tower setups [--json]`: `{"setups":[{"name","account","about","repos":[…],
  "roles":{"<repo>":"<role>"}}]}` (only the repos that have a role). The
  first is the default.
- `tower setup add "<name>" --account <account> [--about "<line>"] --repo
  "<folder>=<its role>" [--repo …] [--was "<old name>"]`: a repo is
  `<folder>=<role>`, or `<folder> — <role>` (an em or en dash or `-`, a
  space each side), or the folder alone (`setups::repo_and_role`). The
  same name again (any case) changes that project in its place; `--was`
  renames one in its place (so the first stays the default), and refuses a
  name another project has. The account is checked against `cca` (none
  given: the first); a repo that is no folder is taken as a project
  folder's name and matched (`projects::best`), and the answer says which
  folder it took. `tower setup remove "<name>"`. Both edit config.toml's
  text, so the rest of the file stays as it is, and refuse a change after
  which the file wouldn't read.
- The full window's **Projects** (the sidebar's foot, beside Record and
  Shots; Settings has a card with the way there) lists them, a card each
  (name, account, about, each repo with its role and folder), with **+ Add
  project**, **Edit** (one form: name, an account pick from `tower
  accounts`, about, the repos one per line as `<folder> — <its role>`) and
  **Remove** (asked once in the card). It saves through `tower setup
  add|remove` and reads `tower setups --json` again.

### `tower launch`

`tower launch "<goal>" [--setup <name>] [--name <space>] [--repo <folder>…]
[--title "<goal>"] [--done-when "<…>"] [--account <a>] [--review|--no-review]
[--dry-run] [--json]`:

- the repos: those `--repo` gives (each must be a folder; nothing else is
  opened); else those of the project `--setup` names; else those the
  goal's first line names (`launch::repos_named`: a word that is a project
  folder's whole name, or a distinctive part of only one folder's name, at
  least five letters; the rest of the goal is never read for them). When a
  saved project has every repo the first line names, its account and roles
  come with them (only the repos named are opened). With none of these,
  the only project, else the first. No project at all is an error that
  says to name the repos in the first line, or how to add one. It never
  asks. With `--repo` and `--setup`, the project is the one it is for: its
  name, account and roles, the repos as given;
- the space's name is the goal's first two words that carry meaning,
  lower-cased, joined with a dash: `Fix the login redirect` is
  `fix-login`. The small words `the a an to for of and in on with my our
  new some` are passed over (`launch::space_name`), and so are the words
  that only ask for sessions (`open a session at …`) and the repos named;
  a first line made of those only names it from the ticket's title after
  it. It gets `-2`, `-3`, … while a space has it (Herdr's, and those the
  tower remembers, closed ones too). `--name` is taken as given; a space
  that has that name already is joined, not made;
- **the space's goal is the ticket's title** (`launch::title_of`): the line
  after the one that only asks for sessions (what follows that line's
  colon, else the next line with words), else the first line; heading
  marks and a `Title:` in front taken off, at most 120 characters.
  `--title` gives it instead;
- **what counts as done** (`launch::done_when_of`): the ticket's
  "Acceptance criteria", "Definition of done" or "Done when" section (a
  heading, `**…**`, or a line ending in `:`): its bullet lines (`-`, `*`,
  `•`, `1.`, `- [ ]`; a line under one continues it; a list goes on after
  an empty line only as a list), or with no bullets its lines up to the
  next empty one, joined with "; ", at most 400 characters; a heading with
  words after its colon is those words. `--done-when` gives it instead (""
  is kept empty);
- the account: `--account`, else the project's, else the default one;
- whether its PRs get a reviewer ("PR review"): `--review` or
  `--no-review`, else as the space has it (on, for a new space). Kept as
  `missions.<space>.review`, and in the receipt as `"review": bool` (in
  words: "Each PR its sessions open gets a reviewer session…", or "Its PRs
  get no reviewer");
- it sets the goal (`agendas`) and the done-when (`missions.<space>.done_when`,
  as `tower space goal --done-when` sets it) and makes the mission, opens
  one agenda (the request) whose words are the whole ticket under the
  title (only while the daemon runs), then runs `tower start --space
  <space> --new-space --account <a> --agenda <id>` once per repo, so each
  session is named `<space-slug>-<repo-name>` (shown `<space> ›
  <repo-name>`, said "the <repo-name> session in <space>"), works in the
  space's worktree of its repo with a dev port (step 10), and has the whole
  ticket as its first task (with the title in front when the ticket doesn't
  have it), that it is the space's goal, when it is done, which part is
  this session's, the other sessions' names, then where it works. The
  introduction between them is the daemon's, as sessions join;
- it prints one receipt:

    ```
    { "space", "goal": the title, "done_when", "made": bool (false: it joined a space), "dry_run": bool, "review": bool,
      "agenda": id or null, "repos_from": "first-line" | "setup" | "repos",
      "sessions": [ { "name", "repo", "address", "account", "worktree", "branch",
                      "port", "task": agenda task or null, "error": text or null,
                      "why": its role in the project, or how it came in } ],
      "assumed": { "setup", "setup_why", "account", "account_why",
                   "name", "name_why", "worktrees": bool, "worktrees_why" },
      "act": the id of the act kept for it (`tower undo <id>`), when the daemon runs }
    ```

    `--dry-run` prints the same plan (no `address`, `port`, `task`) and
    does nothing: no space, no session, no worktree, no file touched. A
    session that didn't open has its `error`; the others still open.

### A ticket's draft

`tower launch "<ticket>" --draft [--setup <project>] [--adjust "<the
user's words>" --prior '<the draft before, as --draft --json printed it>']
[--json]`: the tower's brain reads the ticket and drafts the space, for the
user to confirm or change. Nothing is opened, nothing written.

- **One headless call** (`ticket::argv`): `cca <the default account> -p
  --output-format json --model <[brains] model> --effort low
  --no-session-persistence --strict-mcp-config --settings
  '{"disableAllHooks":true}' --tools ""` (plain `claude -p …` without the
  accounts mod), the prompt on its stdin, in the system's temporary folder,
  in an environment without `CLAUDE_CONFIG_DIR`, `CLAUDECODE`,
  `CCA_ACCOUNT`, `ANTHROPIC_*`, `HC_*` and `CLAUDE_CODE_*`
  (`ticket::left_out`), so the account is the one asked for. It is stopped
  after 90 seconds. It takes about 6 seconds.
- **The prompt** (`ticket::prompt`): the projects (name, account, about,
  each repo whole with its role; with `--setup`, that project only, and
  that the user chose it), every project folder under the roots with one
  line on it (`ticket::describe`: `package.json`'s description, else the
  first heading, with the sentence after it when the heading is only a
  name, or the first sentence of `README.md`, else of `CLAUDE.md`; 6 KB of
  each at most), the ticket, and for a revision the draft before (in the
  answer's shape, `ticket::prior_brief`) and the user's words. It asks for
  one JSON object and nothing else: `{"project", "account", "repos":
  [{"folder", "why"}], "space", "goal", "done_when", "notes"}`.
- **Its answer** (`ticket::parse_reply`): `claude`'s `result` (an
  `is_error`, or a `subtype` other than `success`, is an error with its
  words), the object in it even inside a code fence or words, or a bare
  object.
- **Checked** (`ticket::check`): a folder must be one of the project
  folders under the roots or a saved project's repo, by its path (`~` made
  whole, compared as the file system has it) or by a name only one has;
  any other is left out and named in `dropped`; each once. No repo left is
  an error that says to draft again or name the repos in the first line.
  The project must be one by that name (else dropped), and the one
  `--setup` named wins, with its own account; an account the mod doesn't
  know is dropped (none: the project's, else the default). The goal is its
  first line as a title (at most 120 characters; none: the ticket's own
  title), the done-when one line of at most 400 (none: the ticket's
  acceptance criteria), the notes one line; the name kebab-case, at most
  32 characters, made unique among the spaces.
- **The draft** is the dry run's receipt of exactly that
  (`launch::plan_with` with `--repo`, `--name`, `--title`, `--done-when`,
  `--account`, `--setup`), with `"draft": true`, `"repos_from": "draft"`,
  `project` (or null), `notes`, `dropped`, `took_ms`, each session's `why`
  (the brain's), and `assumed` saying it was the brain's.
- A model error or a timeout: `the tower's brain couldn't read the ticket
  (…): try again, or name the repos in the first line instead ("open
  sessions in eyzflow and mediaroom")`.
- **Launched as confirmed** (`ticket::confirm_flags`): `tower launch
  "<ticket>" --repo <each folder> --name <space> --title "<goal>"
  --done-when "<…>" [--account <a>] [--setup <project>]`. The terminal's
  draft (`--draft` on a terminal, without `--json`) ends with that command.

### New space

The full window's New space (`src/orb/panel/launch.rs`): beside the
sidebar, **the conversation** that makes the space, in the column a
space's conversation has (same width, same input with its voice and files
at its foot); beside that the pane: **Project** (the projects as pills;
none picked: the brain picks; a second click unpicks) and what the tower
will do. The goal, or the whole ticket, is said in the conversation only:
the pane has no field of its own for it.

- **The conversation** ("Conversation · the new space"; once drafted,
  "Conversation · <space>, not launched yet"): You (amber) and The tower
  (cyan) and, for a planned draft, The planner, each with its time, oldest
  first, scrolled to the newest, and what is under way ("The tower's brain
  is reading the ticket…", "…redrafting it…", "Starting the planner…",
  "The planner is answering…", "Launching…"). What is typed or said at its
  foot goes, as its placeholder says:
  - no draft yet ("Paste the ticket, or say what to get done…"): it is the
    ticket (or, with one said already, more of it, a paragraph on), and it
    is drafted; a first line that names the repos is asked as a dry run
    first, and the tower says what Launch will open instead of drafting
    (a note under what the tower will do says so);
  - a draft ("Say what to change…"): it is redrafted with those words
    (`--adjust "<words>" --prior '<the latest draft>'`); the tower answers
    with the new draft and what changed ("Changed: named ns-check (was
    scratch-check)");
  - a plan of the draft that can talk ("Ask about the plan or change it…"):
    to the planner (`tower plan say --as user`), whose answers come in the
    plan's thread and show here.
  The pane's **Draft the space**, **Plan as a page / whiteboard** and
  **Launch** say what they did in it too ("Plan it first, as a page.", "The
  planner is on it…", "The draft didn't come: …", "The launch didn't go:
  …"). In a narrow window the placeholders are shorter ("What to change…").
- **It goes with the space.** Launch hands the conversation (with the
  plan's thread and when the plan was ready) to `tower launch
  --story-file <file>`, which keeps it in the history as the new space's
  `creation` entries (who said it, when; written down, never spoken, in no
  chat) and adds what the launch opened as the tower's last word. The
  space's own conversation then opens with **How this space was created ·
  N messages** (Fold / Show), then "Since it was launched" and the rest.
  Start over drops the conversation with the draft and the ticket (the
  conversation was the only place it showed), and the input waits for the
  next; one that launched is the space's, and New space starts empty.
- **Several at once** (`src/space_drafts.rs`): each new space in draft
  (its ticket, conversation, the brain's draft, the project picked, the
  review switch, the name the tower said) is kept in
  `<data>/new-spaces.json`, written whenever one changes, so it outlasts
  the window, the orb and the tower. Above the pane, **Spaces in draft ·
  N**: a pill each (the space its draft or dry run names, else a name
  from its ticket), the open one lit, "reading…" or "launching…" beside
  one whose command runs. A click opens another (the one open waits, as
  it is); **+ New draft** starts another from nothing; its **×** asks
  "Delete “<name>”? Its ticket and conversation go. Nothing of it was
  opened." (Delete, Keep it), and a plan of it closes. A draft with
  nothing said in it is not kept. Each draft's commands run as
  `draft:<id>` and `launch:<id>` (one launch at a time across drafts), and
  an answer comes to the draft that asked, wherever the user is by then;
  one that comes after the window closed is written to the file by the
  window that asked (a launched draft leaves it, it is not launched
  twice), and an open window takes what was written there within a
  second. A launch takes its draft out of the list; Start over empties
  the open one, which leaves it. The window opens on the draft that was
  open last; the sidebar's New space row says "N in draft". A file that
  doesn't read is kept aside as `new-spaces.bad-<ms>.json`.

- **The first line names repos** (the dry run's `repos_from` is
  `first-line`, asked without `--setup` once the typing pauses): the plan
  as before, its bullets from the receipt (with "Done when: …"), and
  **Launch** is the confirm (`tower launch "<goal>" --json`).
- **Any other goal**: the bullets say what a draft does, and the button is
  **Draft the space** (`tower launch "<ticket>" --draft --json`, with
  `--setup` for a picked project). While it runs: "Reading the ticket…",
  and that it takes a little while.
- **The draft** shows as a card in its place: Space, Goal, Done when, the
  sessions (each repo with its why), Project and account, the brain's note,
  and what was left out. Under it, that what to change is said in the
  conversation (the redraft takes the card's place), then **Launch**
  (exactly that draft, `ticket::confirm_flags`) and **Start over** (the
  draft, its conversation and the ticket are dropped). A draft belongs to
  the ticket as it was: changed, the button is Draft the space again
  (words in the conversation still redraft from the latest draft).
- A tower whose `launch --help` has no `--draft` gets the launcher as it
  was: the first project, its plan, Launch.
- **Review its PRs**: a switch under the draft card, and under the plan of
  a goal whose first line names its repos, on at first, with a line on
  what it does ("Each PR its sessions open gets a reviewer session that
  looks for critical issues only … Merging stays yours."; off: "No
  reviewer: its PRs come to you as they are."). Launch carries it as
  `--review` or `--no-review`.
- Launch's receipt names the space, whose view opens with what was done
  said at its top; an error stays here.
- **Or see the plan first**: under the draft's Launch and Start over,
  **Plan as a page** and **Plan as a whiteboard** (see "The plan, and the
  chat about it" below). While a plan of this ticket is open, the card is
  the plan's draft (the planner may revise it), the conversation goes to
  the planner, Launch carries `--plan <id>`, and Start over closes the
  plan too. A plan of another ticket that is still open (the ticket was
  added to, the window reopened) says so at the top, with **Back to it**
  (its ticket is the launcher's again, said first in the conversation with
  the plan's thread after it, its draft on the card) and **Close it**.
- `HERDER_TOWER_ORB_DRAFT=<file>` (with `HERDER_TOWER_ORB_WINDOW=show:new=<ticket>`)
  shows the draft JSON in the file as the card; `reading` shows the
  launcher reading; `ready` its button before. `HERDER_TOWER_ORB_PLAN=<file>`
  makes the plan in the file (the state's `plan`, as `Plan::view` writes
  it) the open plan of that ticket. `HERDER_TOWER_ORB_PROJECT=new|<name>`
  (with `show:projects`) opens the Projects form once.
  `HERDER_TOWER_ORB_TALK=<file>` (with `show:new`) says the file's lines
  in the conversation one after another, each once the tower is on none
  (`!wait <s>` waits, `!launch` presses Launch, `!new` starts another
  draft, `!open <n>` and `!delete <n>` open and delete the n-th in the
  list from 1, `!reopen [<s>]` closes the window as Esc does and opens it
  again on New space after `s` seconds, 3 if none), printing each to the
  log (`[orb] talk: …`). Its lines are said once for the run: a window
  opened again goes on with the next.

### The plan, and the chat about it

The user wants to see the plan before anything opens, and to talk it
over. Artifacts can only be published by an interactive Claude Code
session (a headless `claude -p` has no Artifact tool), so the plan is
written by one the tower owns: **the planner**.

- **The planner** is a hidden Herdr terminal named `tower-planner`
  (labels `tower.role=planner`, `tower.plan=<id>`), started by the daemon
  the way a brain is (`hc run --bg --no-parent --no-notify`; its first-run
  folder question is the tower's to answer, `brain::settle`): `env
  HERDER_TOWER_PLAN=<id> [the tower's HERDER_TOWER_HOME/STATE/DATA/BIN when
  they aren't the usual ones] cca <the default account> -n tower-planner
  --model <[brains] model> --effort <[brains] effort> [claude_args] "<its
  first task>"` (plain `claude`, without CLAUDE_CONFIG_DIR, without the
  accounts mod). It always runs Claude Code, whatever `brain` says.
- **Its name is a brain's** (`brain::is_brain_name`): never a session of
  the user's, never watched, never introduced, never in a space's members
  or the radar's lists, its dialogs never questions or decisions. It is no
  space's brain (`brains::is_space_brain` and `brains::is_ours` say no: no
  note or request is routed to it, `tower model --restart` leaves it), and
  a space called "planner" has the brain `tower-planner-space`.
- **Its folder** is `<state dir>/plans/<id>/`: `ticket.md`, `draft.json`
  (the draft, as the planner last revised it), `instructions.md` (also its
  `CLAUDE.md`), the `plan.md` it writes, and `.claude/settings.json`: it
  may Read, Glob and Grep anywhere (the draft's repos are its
  `additionalDirectories`), edit only in its folder (`Edit(./**)`; an edit
  in a repo of the draft is denied), run `tower plan …` and `tower setups`
  and read-only `git`/`ls`/`cat`/`rg`, and publish (`Artifact`); it may
  never run `tower launch|start|queue|dispatch|space|answer|agenda|undo`,
  `hc`, `herdr-*`, `cca`, or a `git` that changes a repo, nor ask in its
  terminal (`AskUserQuestion`). Anything else asks, and shows as blocked.
- **Its first task** (`plans::first_task`, `plans::instructions`): write
  the full plan of the draft into `plan.md` (each session: what it does
  and in what order; how the repos fit together: the APIs, data and
  contracts; the risks; the open questions; how it is tested; each session
  under `### <its repo>` in a `## Sessions` section), publish it, and say
  so with `tower plan published <id> --url <link>`. **A page**: one
  self-contained HTML page Artifact, "Plan · <space>". **A whiteboard**: an
  Artifact from the account's Whiteboard type
  (`https://claude.ai/artifact/PLsEyfjXbDUM1iasW7rjLR`): a block per
  session, arrows for the dependencies, a section for the risks and the
  open questions. One artifact only. It can't: `tower plan failed <id>
  "<why>"`.
- **Then it waits.** What the user says reaches it through the send queue
  as a quiet note (`sendq::add_note`: after its turn, never a task, never
  watched): `[plan <id>] The user says: <words>` and how to answer. It
  answers in a few sentences with `tower plan say <id> "…"` (nobody reads
  its terminal). A change the user agrees to: a revised draft (`draft.json`,
  then `tower plan draft <id> --json-file draft.json`), or `plan.md` changed
  and the same artifact republished (its `url`), then `tower plan published`
  again.
- **One plan at a time.** A new plan closes the one that is open; Start
  over, Close it and a launch close it too. Closing ends its planner (`hc
  kill`, the terminal removed).

#### The store: `plans.json`

The daemon keeps every plan in `<state dir>/plans.json` (written before it
answers; the open one and the last 20 closed), and shows the open one in
`state.json` as `plan` (null when none), which the window follows:

```
{ "id": "p3", "form": "page" | "whiteboard",
  "status": "writing" | "ready" | "failed" | "closed",
  "ticket", "draft": the draft receipt (as last revised), "revision": how many times the planner revised it,
  "address": the planner's terminal or null, "planner": "starting" | "working" | "idle" | "blocked" | "gone" | …,
  "blocked": a dialog waits in its terminal, "answering": the user's last line has no answer yet,
  "dir", "plan_md": its folder and plan.md, "url": the artifact's link or null, "why": why it failed, or that the planner ended,
  "thread": [ { "from": "user" | "planner", "text", "at" } ],   (oldest first, at most 200)
  "created_ms", "ready_ms", "updated_ms", "closed_ms", "launched": the space launched with it,
  "prs": [ Predicted ] }   (the PRs it predicts, each with its score: "Predicted PRs and their review score" below)
```

Every two seconds the open plan's planner is looked at (`hc get`): its
status word becomes `planner`; a terminal that has ended is `gone` (a plan
not yet published fails: "the planner's terminal ended before it published
the plan"; a published one stays ready, and says Plan again starts one to
talk it over).

#### `tower plan`

- `tower plan new --form page|whiteboard --draft '<the draft's JSON>'
  --ticket-file <file> [--json]`: the draft must be a draft receipt (its
  `space`, a session with a `repo`); the ticket is read from the file. The
  answer is the new plan (`writing`, the planner `starting`); the planner
  starts a moment later.
- `tower plan say <id> "<words>" [--as user|planner]`: a line of the chat.
  Whose: `--as`, else the planner's when its environment names that plan
  (`HERDER_TOWER_PLAN`), else the user's. The user's is queued to the
  planner (an error when it isn't running); the planner's is its answer.
- `tower plan draft <id> --json-file <file>`: the planner's revised draft,
  in the draft's shape or the shape a draft is asked in (`{project,
  account, repos: [{folder, why}], space, goal, done_when, notes}`),
  checked as a draft is (`ticket::check`: an unknown folder is left out and
  said so; none left is an error) and kept as its receipt; it prints what
  it took.
- `tower plan prs <id> --json-file <file>`: the PRs the plan predicts
  (`{"prs": [{repo, base, title, changes, files, migration, breaking,
  escalate, after}]}`), checked against the draft's repos and scored; it
  prints each one's score, for the planner to put in the plan as it is.
  Again after a change: a PR that opened keeps its prediction.
- `tower plan published <id> --url <link>` (an `https://` link; again after
  a republish), `tower plan failed <id> "<why>"`.
- `tower plan [show [<id>]] [--json]` (the open one when no id; from
  `plans.json` when the daemon isn't running), `tower plan list`, `tower
  plan close [<id>]` (`--launched <space>`: what it became).
- Words after `--` are words (`say <id> -- --json is a word here`).

#### A launch with its plan

`tower launch "<ticket>" --repo … --plan <id>` (New space's Launch with a
published plan) reads the plan (its link, and `plan.md`), and each
session's first instruction ends, after its brief, with "The space's plan
(<link>), as the tower's planner wrote it with the user: follow it, and
say so when you must leave it." and its part of `plan.md`
(`plans::part_for`): all of it when it fits in 6,000 characters, else
without the other sessions' own sections (a heading that names another
session's repo and not this one's), cut at a line, saying where the rest
is. The receipt has `plan: {id, url}` (a `--dry-run` reads the plan and
says it, and opens nothing). Once the sessions are open the plan is closed
(`launched`: the space) and its planner ends. A plan that isn't
published yet when Launch is pressed is closed, and the draft launches
without it.

#### In the window

Under the draft card (`src/orb/panel/launch.rs`), on the state's `plan` of
this ticket:

- **None**: "Or see the plan first:" **Plan as a page** · **Plan as a
  whiteboard** (`tower plan new --form … --draft '<the card's draft>'
  --ticket-file <the ticket, in a temporary file>`), and what it does.
- **Writing**: "The tower's brain is writing the plan… (it reads the repos first: about five minutes)"
  (while the planner starts: "…starting the planner…"), and that it
  publishes to the user's claude.ai account, private.
- **Blocked**: "The planner waits on a question in its terminal
  (publishing the plan may ask for your go-ahead). Answer it there." with
  **Show in Herdr** (the blocked card's way: `tower show`, the terminal in
  a tab).
- **Ready**: **Plan · page · Open** (`open <link>`), a revised draft said
  ("the card above is the draft as it is now"), and that Launch hands each
  session its part. A planner that ended: why, and Plan again.
- **Failed**: "The plan didn't come: <why>." and **Plan again as a page** ·
  **Plan again as a whiteboard**.
- **Pull requests**: once the planner predicted them, under the plan's
  line: "2 pull requests, 1 needs review", then a row each (`PR 1 ·
  <repo> → dev`, its title, what it changes, roughly which files, its
  badge, **Needs review** in amber or **Low risk** in cyan, and why, and
  "After PR 1" / "Opened: <key>"), and who merges which (auto-merge on or
  off).
- **The chat** is New space's conversation (above): the user's lines
  (You, amber) and the planner's answers (The planner), with "The plan is
  ready, a page on your claude.ai account: <link>." among them, in time
  order; "The planner is answering…" while the user's last line waits; its
  input says "Ask about the plan or change it…" (`tower plan say --as user
  <id> -- "<words>"`). Under the plan's line in the pane, that it is
  talked over there.

## Step 10: every mission gets its own worktrees

The code is `src/worktrees.rs` (`tower dispatch`'s `isolate` makes its
worktree with the same `worktrees::add`).

- Only a **mission** gets worktrees: a space that has a goal, or that
  `tower launch` made (`missions.<space>.setup`), and that isn't standing
  (`missions::works_in_worktrees`). A space with no goal is the user's own:
  a session opened there works in the repo itself, with no port handed
  out, as before the Quiet Tower. Setting a goal (`tower space goal`) makes
  it a mission from the next session on; clearing the goal undoes that.
- When the tower opens a session for a mission (`tower launch`, `tower
  start --space …`, a `tower dispatch` task with a `space` that starts a
  session), `[spaces] worktrees` is true, the
  session runs on this Mac and its folder is in a git checkout, the session
  runs in a git worktree: `<repo>/../.tower-worktrees/<space-slug>/<repo-name>`
  on the branch `<space-slug>/<repo-name>`, made from the repo's HEAD
  (`<space-slug>`: the space's name lower-cased, anything but `a-z 0-9 _`
  a dash). A worktree or a branch that is there already is used as it is,
  so a second session for the same repo in the same space shares the
  worktree. `--cwd` inside the repo starts the session in the same folder
  inside the worktree. `.tower-worktrees/` ignores itself (its own
  `.gitignore`).
- The repo's untracked top-level `.env*` files are copied into a fresh
  worktree (never over a file that is there).
- The session gets a dev port from `[spaces] port_range` (default
  `"3000-3050"`): the first that no live session was given, passing over a
  port something listens on now while another is free.
- Its first instruction ends with: "You work in a fresh worktree of <repo>
  at <path> on branch <b>; dependencies may need installing (<files>
  copied from the repo); use port <p> for a dev server." (for a worktree
  that was there: "You work in this space's worktree of <repo> at <path> on
  branch <b>, which was there already; use port <p> …").
- What it was given is kept on its terminal as Herdr labels
  (`tower.worktree`, `tower.branch`, `tower.repo`, `tower.port`), so it
  lives and ends with the terminal; the daemon copies it to the session in
  its registry (`tower sessions`: `worktree`, `branch`, `port`) and
  `missions` shows it. `tower start` prints `worktree`, `branch`, `repo`,
  `worktree_fresh` (it made that worktree now) and `port`. The space's
  mission lists the worktrees made for it.
- **A space of its own in Herdr's sidebar.** Herdr's sidebar (its
  `spaces` mod) places a space by its first terminal's live folder, and
  nests a space whose place is a git worktree under the space whose place
  is that repo's own checkout (a fork, with a branch icon). A space the
  tower makes for a session in a worktree (`tower launch`, `tower start
  --space … --new-space`, and a space `tower reopen` makes again) opens
  on a **home tab** first: a shell named after the space, in
  `.tower-worktrees/<space-slug>/` beside its worktrees (in no checkout),
  labelled `tower.role=home`; the sessions are the tabs after it. It is no
  session (no agent: never in `missions`, never kept by `crate::reopen`),
  so undoing a launch still closes the space whole. Closing that tab
  brings the nesting back. Spaces made before this keep their place.
- `tower start --no-worktree` opts one session out (it gets no port
  either). A space with no goal, a standing space, a start without
  `--space`, a folder that is no git checkout, `--on <machine>`, or
  `worktrees = false` behaves as before. If the worktree can't be made the session opens in the folder
  itself and the answer says so.
- `tower dispatch` takes a session in a space's worktree as the one "in
  the project's folder": for a task that names a space, that space's
  worktree first, then the project's own checkout; for a task that names
  none, the checkout first, then any worktree of it. An `isolate` task
  keeps its own worktree (`<project>-<name>`, branch `tower/<name>`), not
  the space's.
- Closing the space removes its clean worktrees (step 5 and 8).

## Merge all: the tower merges the chain

A merge card's button names the PRs that can go cleanly ("Merge all 3").
For one PR it asks, and "Confirm" merges it from the window, as before.
For several, the one confirmation ("Confirm: merge all 3 in order") is
for the whole chain, and the daemon merges it (`crate::merges`,
`daemon::merges`; `{"cmd":"merge_chain","action":"start","card":…,"prs":[keys],"by":"mac"}`),
each chain on a thread of its own with a look every 15 s:

- **In order, one at a time.** The next PR starts only once GitHub says
  the one before is merged.
- **Waits are not failures.** Right after a merge GitHub works out again
  whether the next one merges cleanly (`mergeable` / `mergeStateStatus`
  UNKNOWN): waited for, up to 15 minutes. Checks running: up to 45.
- **Fixed by its session.** A PR that fell behind its base (`BEHIND`: the
  repo wants branches up to date) or now conflicts with it (`DIRTY`), or
  whose checks fail, gets one request through the send queue to its
  session: bring the branch up to date (merge the base in, resolve the
  conflicts, run the tests), push, and leave the merge to the tower. One
  that only fell behind and has no session left is brought up to date on
  GitHub (`gh pr update-branch`). Its push is waited for (up to an hour),
  then GitHub and its checks, then it is merged at its new head. The rest
  of the chain is never put back to the user for it.
- **Back to the user, once.** Only what can't be fixed: checks that still
  fail after its fix, conflicts still there after it, a draft, a rule of
  GitHub's that blocks it (a required review), a merge GitHub refuses three
  times, or a wait that ran out. The chain stops there; the PRs after it
  stay on the card, which says why ("Merging in order stopped: #1376 back
  to you: …"), and a line goes into the record (kind `merge`, status
  `back`).
- Each merge is the repo's own method, pinned to the head commit looked
  at, never `--admin`; one that lands is reported as a merge from the Mac
  (`Prs::report_merged`): its chip moves, its session and the brain are told.
- While it runs, each row of the card says where its PR stands ("GitHub is
  working out whether it merges cleanly", "its session is bringing its
  branch up to date with dev", "merged"), and the card's one button is
  **Stop merging** (`"action":"stop"`: nothing more is merged). The state
  carries every chain as `merges` (`{id, card, space, by, links: [{key,
  number, base, step: {step, why}, line}], over, stopped}`), the ones that
  ended for an hour.
- A daemon restart ends a running chain (what merged stays merged); the
  card's button starts it again with what is left.

## PR review: every PR gets a reviewer, and the two agree

The model is `src/reviews.rs`; the daemon runs it in `src/daemon/reviews.rs`
and keeps it in `reviews.json` (the state folder) across restarts.

### The switch

A space's `review` (`missions.<space>.review`, on unless the user turned
it off; a mission from before reads as on): `tower space review <space>
on|off`, the space's **Review PRs** switch, New space's **Review its PRs**
(`tower launch --review|--no-review`). A closed or archived space's PRs
get no reviewer. Turning it off ends a reviewer that is open there.

### A PR found

The tower finds a PR when a session's turn prints its link (`prs`). When it
is new, open, of a session in a space with review on, the tower opens ONE
reviewer for it (`Book::wants_reviewer`): never for a PR a reviewer opened
(by the store, or its terminal's label `tower.role=reviewer`), never twice
for one PR (a review is kept before its reviewer opens, and the record of
one that is over still counts).

- **Who**: `tower start --no-act --no-watch --no-track --no-worktree --name
  <name> --cwd <folder> --space <space> [--account <the owner's>] "<its
  first task>"` (this program, `HERDER_TOWER_BIN` when set), then labelled
  `tower.role=reviewer` and `tower.review=<key>`. Same space, same account
  as the PR's session (its **owner**).
- **Its name**: `<space>-review-<repo>-<n>` in Herdr's name alphabet, cut to
  48 characters as session names are (the space's part gives way first, the
  number stays), never `tower-…` (`review-<repo>-<n>-<space>` then), made
  unique among the live terminals (`reviews::reviewer_name`).
- **Where**: a git worktree of the owner's repo (the repo the tower gave it
  a worktree of, else the checkout its folder is in) detached at the PR's
  head: `<repo>/../.tower-worktrees/<space>/review-<repo>-<n>`. The head
  commit is the PR's `headRefOid` when the repo has it (the owner pushed it
  from here), else fetched from the remote that is the PR's repo
  (`pull/<n>/head`, then its branch), never from another one. When there is
  no checkout to be had, a folder of its own (`<state>/reviews/<name>/`),
  where it reads the PR with `gh pr diff`. Either way a
  `.claude/settings.local.json` there lets it read, comment, `tower review`
  and `tower queue`, and denies `git push|commit|merge|rebase|reset`, `gh pr
  merge|close|edit|ready`, Edit and Write: on no branch, it can't push to
  the owner's.
- **Its first task** (`reviews::instructions`): the PR (link, title,
  branches), its owner (name, address), the space's goal and done-when, its
  folder, and the review: CRITICAL issues only (wrong behaviour against the
  ticket or goal, a crash, security, data loss or corruption, a build or
  test failure it introduces, an API or contract break with another repo;
  nothing about style, naming or nits). Each one a PR review comment opening
  with `Critical:`, inline where it can (`gh api repos/<repo>/pulls/<n>/comments
  … commit_id=<head> path line side=RIGHT`, else `gh pr comment`); the list
  and the links to the owner itself (`tower queue <owner> "…"`); then
  `tower review report <key> --critical <n> "<one line>"`. None critical:
  approve at once (`gh pr review --approve`; on a PR of its own account,
  which GitHub won't approve, `--comment "Approved: …"`), tell the owner,
  `tower review agree <key>`. Told it's fixed: check again (`gh pr checkout
  <n> --detach`), then agree or post what is still critical and report
  again. It never merges, closes, pushes or edits the owner's code, and asks
  the user nothing.
- **Introduced**: the introductions (`introduce`) take it as a newcomer;
  its task there is "reviewing PR #n (<repo>) for critical issues only".
  Its turns' ends make no PR chip, card or question (`asks`).

### The rounds

The tower passes nothing between the two: they talk directly (`tower
queue`, a peer message). It keeps the record and escalates.

- `tower review report <pr> --critical <n> "<line>"` (the reviewer): `n` >
  0 is a **round**. The status is `changes`, and the owner gets ONE quiet
  note (`sendq::add_note`): "[tower] The reviewer (<name> <address>) left N
  critical comments on PR #n (<repo>): fix them, push, answer each thread,
  then tell the reviewer (tower queue <address> "…"). If you like, tell the
  tower too: tower review fixed <key> "<one line>"." Nothing of the
  comments themselves. `n` = 0: back to `reviewing`, to agree next.
- `tower review fixed <pr> "<line>"` (the owner, optional: the next report
  is what counts): `reviewing` again, the line kept.
- **Stuck**: the third round with no agreement is `stuck` and ONE decision
  for the user (raised with its options: kind `choice`, `why` `review`, key
  `raise:<space>:<line>`, about both sessions): "The reviewer and the owner of PR #n (<repo>) don't
  agree on: <the last report's line>", with the PR's link, the round and
  who is who as its detail, and "Side with the owner" (the change stands;
  the reviewer approves and agrees), "Side with the reviewer" (the owner
  fixes what is still critical; the reviewer checks again) and "I'll look"
  (both leave it until the user says more), no pick. The answer's words go
  to both sessions (step 3). No note to the owner while it waits; a round
  after the user answered notes the owner again; never a second decision.
- `tower review agree <pr> ["<line>"]` (the reviewer): `agreed`, over.

### The end

Agreed, the PR merged or closed, the space closed or archived, its review
turned off, the PR's own session gone (nobody is left to fix it; the PR
stays the user's to merge), its reviewer's terminal gone, or a reviewer that never opened
(five minutes on, none under its name): the review is over (`ended` with
why, unless agreed). Its reviewer session ends (`hc kill`, `hc rm`), its
checkout is taken away (`git worktree remove --force`: nothing in it is
anyone's work), and a decision still open about it is settled. The record
stays. Every 20 seconds the daemon looks at each review not over, and at
once after `tower space review … off`.

### The store and the commands

`reviews.json`: `{"reviews": [Review]}`, every review not over and the last
50 over:

```
Review { "key": "owner/repo#12", "repo", "number", "url", "title", "base", "head", "head_sha",
         "space", "owner", "owner_name", "owner_created_ms", "reviewer", "reviewer_name", "reviewer_created_ms",
         "account", "folder", "checkout_of",
         "status": "reviewing" | "changes" | "agreed" | "stuck" | "ended",
         "rounds": n, "critical": n, "line": the last word on it, "fixed": the owner's last word,
         "decision": the stuck decision's id, "created_ms", "updated_ms", "ended_ms",
         "steps": [ { "at_ms", "what": "opened" | "started" | "report" | "fixed" | "stuck" | "agreed" | "ended",
                      "by", "critical", "text" } ] }
```

- `tower review [list]`, `tower review show <pr>` (JSON when piped or with
  `--json`; from `reviews.json` when the daemon isn't running):
  `{"reviews": [Review + "chip"]}`, `{"review": Review + "chip"}`. A PR is
  its key, its link, or `#12` (the caller's own, else the only one open).
- `tower review report <pr> --critical <n> "<line>"`: `{"review",
  "noted_owner": bool, "decision": id or null}`; `agree <pr> ["<line>"]`:
  `{"review", "ends": the reviewer's address}`; `fixed <pr> "<line>"`:
  `{"review"}`. A review that is over refuses them.
- The socket: `{"cmd":"review","action":"list"|"show"|"report"|"agree"|"fixed"|"sweep","pr","critical","text","from"}`.
- History: kind `review` (`id` the PR's key, `status`, `record: true`):
  opened, each round, fixed, agreed, ended. Never a chat message.

### What shows it

- **The PR** (`prs.json`, the chat's chip, `GET /v1/prs`, the `pr` event)
  carries `review`: `{status, rounds, critical, reviewer, reviewer_name,
  line}`, `line` being what the chip says (`reviews::chip_line`):
  "Reviewing…", "Reviewing the fixes… (after round 1)", "Review: 2 critical,
  round 2", "Review: stuck after 3 rounds, yours to decide", "Review:
  agreed", "Review: ended without agreement" ("…, N critical left").
- **The merge card**: each PR's row has that line (amber until agreed); a
  merge decision's `prs[]` carry `review`, and a review that isn't agreed
  is among the merge's `warnings` (`prs::gate`, `reviews::merge_warning`):
  "The review isn't done: its reviewer is still reading it.", "The review
  isn't agreed: 2 critical issues open, round 2.", "… the reviewer and the
  owner disagree after 3 rounds.", "Its review ended without agreement."
  The merge stays the user's: it never blocks it.
- **The state** has `reviews`: every review not over and those over in the
  last day, the latest first (`Review` + `chip`).

## Predicted PRs and their review score

The model is `src/prscore.rs`; the tower's own merges run in
`src/daemon/automerge.rs`.

### The plan predicts them

When a plan changes code, its planner says what the future pull requests
will look like, one entry per PR, in `prs.json` in its folder, and hands
them over with `tower plan prs <id> --json-file prs.json`:

```
{ "prs": [ { "repo": "<the repo's folder name, as the draft has it>", "base": "dev",
             "title", "changes": "<what it changes>", "files": ["<roughly which files>"],
             "migration": false, "breaking": "<what it breaks, or empty>",
             "escalate": ["<anything else on the escalate list>"], "after": [<the PRs it waits for, by number>],
             "checks": [ { "id": "c1", "what": "<what it shows>", "how": "<the exact test, command or live step>",
                           "evidence": "<test output, a screenshot, a log, the deployed state>" } ],
             "edge_cases": [ { "case": "<the edge case>", "checks": ["c1"] } ] } ] }
```

`base` is `dev` when not said: the user's release rule is dev first, then
dev into main or prod as a release PR of its own, which the user merges.
The tower checks each (its repo is one of the draft's; it has a title; what
it waits for is in the list; 30 at most; at least one check, each with its
what, how and evidence, ids unique; at least one edge case, each naming a
check of its PR), numbers them from 1, scores them
and keeps them with the plan (`prs`). It prints the scores; the planner
puts a `## Pull requests` section in `plan.md` and in the artifact, one
entry per PR with its score and reason as printed. A launched session's
first instruction carries that section with the rest of its part of the
plan.

### The score

A PR **needs review** when a person must read it, and is **low risk**
otherwise. It needs review for:

- **a database migration**: the planner says so (`migration`), a file in a
  `migrations/` (`migrate/`, `alembic/`, …) folder, a `.sql` file,
  `schema.prisma`, or words such as "add column", "drop table", "backfill";
- **a breaking change**: the planner says what breaks (`breaking`), an API
  contract file changes (`openapi.*`, `swagger.*`, `.proto`, `.graphql`),
  or the words say it ("breaking", "changes the response", "renames the
  total field", "removes the v1 endpoint": a verb that takes something
  away, then within four words a thing others rely on);
- **anything on the escalate list**: secrets or credentials (`.env`,
  `.pem`, "api key"), infrastructure (Terraform, `.github/workflows/`,
  Docker, `serverless.yml`, "AWS", "DNS"), a deploy or release ("release:
  dev into main"; a release build isn't one), auth (`auth/`, `sign-in`,
  "permissions"), deleting or overwriting data, money or billing, new
  dependencies (a dependency manifest), security; and whatever the planner
  lists in `escalate`.

A word said as not done ("no migration", "without breaking changes")
doesn't count. Each reason names what it was found in; the line is
"Needs review: a database migration (db/migrations/0042_orders.sql)" (two
reasons, then "and N more"), or "Low risk: no migration, breaking change,
secrets, infrastructure or auth".

### The real PR carries it

When a session's turn opens a PR (`prs::Prs::found`), the daemon scores it
before it is shown (`score_found`): the plan its space was launched with
(the latest) is asked for the prediction it is (`plans::Book::match_pr`:
the same repo, not matched yet, the same base first, then the title that
shares the most words, then the first in order; a PR found again keeps its
prediction), and the PR's own title and files at its head are read (`gh pr
view --json files,headRefOid`). The score is the prediction's with what the
files show added; what they show and the plan didn't is said so ("…, which
the plan didn't predict"). A PR no plan predicted is scored on its own
title and files. A push since (its head moved) is read again at the next
look. The score is on the PR (`score`, in `prs.json`, `GET /v1/prs`, the
`pr` event), on the merge card's row and in a merge decision's `prs[]`; a
PR that needs review has its line among the merge's warnings and is never
the tower's pick; the space's view adds the word to each PR's line.

### How each PR is validated

The plan's validation is what gives the user the confidence to merge and
release, so it is part of every PR from the plan on (`prscore::Validation`):

- **In the plan.** Each predicted PR has its `checks` (what it shows, how:
  the exact test to add or run, the command, or the live step such as
  running it, opening it or checking the deployed state on dev, and the
  evidence it produces) and its `edge_cases`, each tied to the checks that
  cover it. The planner puts them in `plan.md` and the artifact beside the
  score, and gives each session (each task) a `#### Validation` and
  `#### Edge cases` of its own. New space shows them under each PR's row
  (VALIDATION · n, EDGE CASES · n).
- **In the launch.** A launched session's first instruction lists the PRs
  the plan predicts for its repo, with their checks and edge cases, and how
  to report each (`plans::checks_text`).
- **On the real PR.** The PR matched to its prediction carries them
  (`validation`: each check `planned`, until reported). Its session reports
  each with `tower validate <pr> <check> passed|failed|not-run --evidence
  "<the test output, a screenshot's path, a log, or why it wasn't run>"`
  (`<pr>`: its key, its link, or `#12`, its own; a PR the tower doesn't know
  yet is looked up on GitHub first, as the caller's). `tower validate <pr>`
  shows where they stand. When the session finishes its turn on the PR
  (its turn's end finds the PR, `waiting`), what it hasn't reported is
  marked `not_run` ("not reported when its session finished its turn on the
  PR") and the session gets one quiet note listing them and how to report
  them; a later report replaces it.
- **On the merge card and in the decision.** Each PR's row says "All 3
  planned checks passed", or "Checks missing: c2 … (not run), c3 … (failed)
  (1 of 3 passed)" (amber; rose when one failed); a merge decision's
  `prs[]` carry `validation`, its option says what is missing and is never
  the tower's pick, and the missing checks are among the merge's warnings.

### Breaking changes: who depends on what it changes

A PR check also establishes whether the change breaks other people's
functionality (`src/impact.rs`):

- **What it changes** that others can use, read from its diff (`gh pr
  diff`): public functions, types and constants it removes, renames or
  whose parameters it changes (`export` in JS/TS, `pub` in Rust, a capital
  in Go, a module-level name in Python, what isn't `private` in
  Kotlin/Java/Swift); API routes it no longer has; fields of what an API
  sends or takes (in API-like files: `api/`, `routes/`, `handlers/`,
  `serverless`, `schema`, `types`, `dto`, …; distinctive keys only); DB
  columns a migration drops or renames; config keys it stops reading;
  events it stops sending.
- **Who uses it**: every git checkout under `[prs] impact_roots` (default
  `~/Work`; a worktree is its repo's), searched with `git grep` (whole
  words; a route by its fixed part): the PR's own repo outside the files
  the PR changes, and every other repo (the dashboard that calls a
  serverless API, the SDK, the mobile app). Docs, lockfiles and built
  files don't count; nor does a definition of the same name. Each use is
  named: repo, file, line, and the function it is in.
- **The reviewer** (when the space has one) is told to check them: compile
  or type-check them, run their tests against the change, search for what
  the tower missed, and report: `tower impact <pr> breaks "<what breaks,
  who uses it>"` or `tower impact <pr> clean "<what it ran>"`. `tower
  impact <pr>` lists what the tower found.
- **A breaking change** (anything found, or a break reported) makes the PR
  "Needs review: a breaking change (renames formatTotal to formatAmount,
  used by shop-web src/cart/Total.tsx:4 in renderTotal)", with the warning
  "Breaking change: …" on its merge card row (rose), among its merge
  warnings, in its merge decision (`prs[].impact`) and for the phone; the
  tower never merges it. Nothing found: "Nothing outside depends on what it
  changes (…; searched N repos)". A push is read again before any merge.
- **In the plan**: each predicted PR says who outside depends on what it
  changes (`dependents`: what, used by whom, `breaks`) or how the planner
  checked that nothing does (`outside`); a dependent it breaks makes it a
  breaking change in the plan already, shown under its row ("⚠ Breaks").

The search is by name: a same-named thing that isn't the same one is a
false alarm, which sends the PR to the user; never the other way.

### The release track: dev and prod

A space's work goes into dev (and is deployed there), then to prod, and
that can happen several times in one space. Each space has a **release
track** per repo (`src/releases.rs`, run by `src/daemon/releases.rs`),
worked out from GitHub and git, never from what sessions say:

- **Its changes**: the PRs the tower tracks in that space (found in the
  last two weeks). Each one's stage: `open`; `merged` into dev (when); `deployed`
  on dev (dev's last successful GitHub deployment, its ref the dev branch or
  its environment dev, development or staging, has the PR's merge commit);
  `released` to prod (a release PR that landed has it); `closed` without merging.
- **Its releases**: the repo's release PRs, from a dev branch into a prod
  one (`gh pr list`, and the release PRs the tower tracks), what of the
  space each carried and when it landed. A change is on prod since the first
  release whose head contained its merge commit (GitHub's compare,
  `repos/<repo>/compare/<commit>...<release head>`: `ahead` or
  `identical`), however that release merged into main.
- **The user's rule** is dev first, then dev into prod: a PR merged straight
  into main is on prod and said so: "#850 went to main without dev first,
  against the dev-then-prod rule".
- **What it says**, per repo, a line each: "On main since 14:20: #829,
  #830" (one per release, newest first), "On dev, not on main: #831",
  "Release under way: #841 dev → main, carries #831", "Open: #832"; and per
  change its stage with when ("on main since 14:20 (release #840)", "on
  dev (deployed since 14:35)", "merged into dev since 14:05, not deployed
  yet").
- **Where it shows**: the space's view (a "Release track" card: the lines
  in colour, on prod cyan and on dev amber, each change with its stage chip,
  each release with what it carried), the space's row in the sidebar ("2
  of 3 · prod 14:20 · 1 on dev"), `state.json` (`releases`, by space), the
  phone (`GET /v1/releases`, feature `releases`), and `tower releases [<space>]`.
- Every 90 seconds, asking GitHub as little as it can: one PR list per
  repo (its release PRs, and every recent PR's merge commit and time, so a
  PR found before the tower kept those needs no call of its own); a compare
  only against a release that landed after the PR merged, the likely ones
  asked eight at a time; what can't change (a commit in another's history,
  a merge commit) kept in `releases-cache.json` in the state folder, so a
  restart starts warm. Measured on the user's tracked PRs (9 spaces, 3
  repos): about 12 s the first time, about 4 s after a restart.

### Merging without asking

`[prs] auto_merge_low_risk` (off by default; `tower automerge on|off|status`,
Settings → Pull requests). While it is off every merge is the user's, as
before. While it is on, the tower merges a PR itself (`prscore::auto_path`)
only when all of these hold:

- a plan predicted it (`score.planned`), and it is scored low risk, read on
  its own files at its head commit;
- who depends on what it changes was checked at its head commit, and
  nothing breaks (no breaking change);
- every check its plan lists for it passed, with its evidence: one not
  reported yet is waited for (no decision meanwhile), one failed or not run
  makes it the user's, and a planned PR with no checks is the user's;
- it merges into dev (`dev`, `develop`, `development`): never main or prod;
- it is open, no draft, without conflicts, and GitHub says it merges
  cleanly;
- its checks pass (none running, none failing; a PR with no checks is the
  user's);
- its reviewer agreed without finding anything critical (agreed in round
  0): a PR with no reviewer, one whose reviewer found something (even if
  fixed since), or a review that ended without agreement is the user's;
- the tower hasn't failed to merge it at this commit.

Every ten seconds the daemon's PR thread looks (`AutoMerger::tick`): a
ready PR's files are read once more right before the merge (a migration
that came in with the last push stops it), then `prs::merge` merges it
pinned to that head commit, with the repo's own method, never `--admin`.
GitHub is asked, `merged_by` is `tower`, `auto` says how it went, and the
session and the brain are told as after any merge ("The tower merged PR #N
(…) into dev itself: low risk, into dev; its checks passed and its
reviewer found nothing."). A merge GitHub refuses (branch protection, a
required review) is kept as `auto: failed` with why: the PR is the user's
from then on, until a new commit. While a PR is on its way (checks
running, its reviewer reading, or ready) it is no merge decision: it waits
for nobody; once it falls off the way (checks fail, the reviewer finds
something, its files need review), its decision comes as before.

## What you type in a session and what the tower queues for it

You talk to a session in two ways: through the tower (a request, a card's
answer, the brain's instruction, the tower's own word such as "PR #787 was
merged on GitHub"), and directly, typing in its terminal in Herdr. The two
are kept in step.

**The tower doesn't repeat you.** What you type in a session's own terminal
(its hook's prompt, when it is yours: `typed::whose`) goes to the send queue
(`SendQueue::user_said`), kept half an hour. Every message of the tower's
waiting for that session (not a note, not a message from another session,
not a session to start) that was queued after those words, or up to ten
minutes before them, is checked against them before it goes in
(`covered::judge`):

- A merge note is covered by a rule: you said "merged" there yourself, and
  named no PR or this one ("merged on dev" covers "PR #787 (eyzhub/eyzflow)
  into dev was merged on GitHub.").
- Anything else, one headless call (the default account, `[brains] model` at
  low effort, no tools, as a ticket draft is read): your messages, numbered,
  and the waiting ones, with their ids. It names the ones your words cover:
  you told the session the same thing, gave the same instruction, or made it
  moot. Anything that adds what you didn't say is not covered; unsure is not
  covered. About 3 to 4 seconds.

The check starts the moment you type (the session works on your words
meanwhile); a message queued a little after your words waits for it before
it goes. A covered message leaves the queue: the record says `Not sent: you
had told it yourself ("<your words>"): <the message>`, its task in the agenda
is dropped with that reason (`QueueFact::Covered`), and the brain hears it
unless the tower is quiet. A check that fails, or takes longer than two
minutes, lets the messages go: nothing is lost on a guess. Each message is
checked against the same words once.

**Your card answer isn't lost to words about something else.** When you
answer a session's question on a card while it works, the tower holds the
answer for the end of its turn. If you then type in that session's terminal,
the held answer is dropped only when your words answer the same question
(the same answer, or another one): the very words, or the same check as
above (`covered::answers_held`). A link to keep in mind, or new work,
leaves the held answer to go as it would; a check that fails keeps it too.
What you typed is written down either way: as new work, or, when it
answered the question, as a `request` with status `answer` (no agenda).

**One thread for it all.** A space's conversation column has, beside its
requests and answers, what passed between you or the tower and each of its
sessions, in order (`talk::session_said`):

| Row | From the history |
|---|---|
| You → eyzflow · in its terminal | a `request` from `terminal:<address>` |
| You → eyzflow · answered its question, in its terminal | the same, with status `answer` |
| You → eyzflow · answered its question | a `request` from `answer:<address>` |
| Tower → eyzflow · queued / sent | the queue's `queued`, then `delivered` (one row, its word changed) |
| Tower → eyzflow · not sent: you had told it yourself | the queue's `not-sent` |

At its end, dashed, is what still waits for the space's sessions: "waiting:
it goes in when its turn ends", "held: checking it against what you typed
there", or "parked". A session's own view has the same thread for that
session alone ("Between you, the tower and eyzflow"), above its screen.

In `state.json`, each agent has `waiting`: what waits for it in the send
queue, in order, `[{"id", "text", "from"?, "note"?, "parked"?, "checking"?}]`
(`from`: another session's name; `note`: an introduction; `checking`: held
for the check of your words).

## Step 7: a tower per open space

The model is `src/brains.rs`; the daemon runs it in `src/daemon/brains.rs`.
A "brain" below is a tower's own agent session: the main one is the
terminal `tower`, a space's own is `tower-<space-slug>`.

```toml
[brains]
per_space = true              # false: one brain for everything, as before
model = "claude-opus-5-5"     # see "The brains' model"
effort = "high"
```

(The table is `[brains]`, not `[brain]`: config.toml already has `brain =
"claude" | "codex"` as a word, and a table of that name would be the same
key twice, which no build could read.)

### Names, folders, terminals

- A space's brain is the Herdr terminal `tower-<space-slug>`
  (`brains::name_for`): the slug worktrees and launch use (`worktrees::slug`:
  lower case, `a-z 0-9 _`, a dash for every run of anything else). Herdr's
  names end at 32 letters, so a longer one is cut to 27 and gets `-` and 4
  hex digits of its own. A space called "voice" gets `tower-voice-space`
  (`tower-voice` is the daemon's terminal).
- It runs in its own folder, `<config dir>/brains/<name without tower->/`
  (`~/.config/herder-tower/brains/sandbox/`; under `HERDER_TOWER_HOME` in a
  sandbox), beside the main brain's `brain/`. The folder holds:
  - `CLAUDE.md` and `AGENTS.md`: the main brain's instructions (the same
    core and behaviors, at the level in force), with one line at the top
    saying whose file it is, and at the end the section "Your space":
    the space's name, its goal and done-when (or that it is standing, or
    has no goal), its sessions (address, name, status, worktree, port) and
    its open requests as they stood when the file was written, how to see
    them now (`tower missions`, `tower agenda`, `tower decisions --space`),
    how to read its thread (`tower thread "<space>"`), and the rules: it
    handles this space only; what crosses spaces goes to the main tower
    with `tower queue tower "…"`; sessions are opened only in this space;
    it never closes the space or starts a tower; its instructions are this
    file. No other space's sessions or requests are in it.
  - `.claude/settings.json`: the main brain's hooks and permissions, with
    its name on the two hooks that bring its words (`tower hook claude
    tower-<slug>`, `tower hook claude-notify tower-<slug>`), `brain-guard`,
    and no activity hooks (the orb's working line is the main brain's).
    For Codex: `notify=["tower","hook","codex","tower-<slug>"]`.
  - `brain.json`: `{"name", "space", "mission"}` (`mission`: the space's
    row of `missions`, as the section was written from).
  The folder is written at each start, again at every look while the brain
  runs (only when something changed; the brain isn't told), and by `tower
  install`, `tower tell` and a behavior change (then every running brain
  gets `[behaviors] …: read <its own file> again, and write nothing`, each
  with its own path; the brain that made the change isn't told).
- It is started like the main brain: a hidden terminal (`hc run --bg
  --name tower-<slug> --no-notify --no-parent --cwd <its folder> --label
  tower.space=<space> -- <program> …`), tied to its space by the label
  `tower.space`. It is not a tab of its space: Herdr's `tab.new` shows the
  tab it makes (it would take the window, and what the user is typing),
  and a tab's terminal has no name to write to. `tower show tower-<slug>`
  puts it in a tab. Its first-run "trust this folder?" question is
  answered by the tower, as the main brain's is.

### When a space has one

Every 20 seconds, and at once when a session starts or ends (with the
missions' look), `brains::step` decides:

- **Start**: the space has at least one live agent session (as missions
  count them: an agent, in the space, not a brain), it isn't closed or
  archived, `per_space` is on, the main brain runs, and no brain of that
  space runs. A standing space and one with no goal get one too. A space
  `tower launch` made gets its own when its first session reports.
- **Stop**: the space is closed or archived (at once); Herdr's window is
  open and no longer has the space (at once: the user closed it in Herdr);
  the space has had no live session for 3 minutes (`IDLE_GRACE_MS`, so a
  session that restarts doesn't bounce it); `per_space` was turned off.
- **Again, later each time**: a brain that isn't there while its space is
  due one is started again after 15 s, 30 s, 1, 2, 4, 8, then every 10
  minutes (`backoff_ms`); one that ran 5 minutes is answered at once the
  next time.
- `tower space close` ends the space's brain before anything else (through
  the daemon, which also holds it off for 2 minutes; with no daemon, by
  its name), so Herdr's `space.close` then closes the whole space as
  before. Its receipt gains `"tower_ended": ["tower-<slug>"]`.
- A daemon that starts finds the brains that run again (the radar's first
  look), so `tower restart` leaves them running.

### Who gets what

`brains::route(space, per_space, known brains)`: the brain of that space
when it **can take it** (its terminal is live and reports `idle`, `working`
or `done`), else the main brain. So everything about a space with no brain,
with one that is starting (no status yet), waits on its own screen
(`blocked`), is in an error or has ended, and everything when `per_space`
is off, goes to the main brain. If handing it to a space's brain fails at
that moment, it goes to the main brain too. Nothing is dropped.

| What | Its space | Goes to |
|---|---|---|
| `[watch]`, `[queue]`, `[agenda]` (the radar's outbox) | the session's (`where`), as the radar knows it | that space's brain, else main |
| `[space]` (a space is ready to close) | the space | that space's brain, else main |
| `[question]`, `[question-text]` | the space of the session the note names (`brain::session_in`) | that space's brain, else main |
| `[session]`: a session runs `tower queue tower "…"` | the sender's | the sender's own space's brain; from a space without one: main |
| `[session]`: a session runs `tower queue tower-<slug> "…"` | | that brain, as addressed |
| `[session]`: one brain writes to another (`tower queue tower "…"` from a space's brain; `tower queue tower-<slug> "…"` from the main one) | | as addressed, at every level; it arrives as `[session] From <space> › tower <slug> (<address>): …` (from the main one: `From tower (<address>): …`) |
| `[behaviors]` | | every running brain, each told its own file |
| `tower ask "<text>"`, the voice, the floating panel | none | main |
| `tower ask --space <space> "<text>"` (a space's view) | the space (its whole name in any case, else a part of it) | that space's brain, with `[radar]` listing that space's sessions only; else main, as `In <space>: <text>` |

- **History.** A request a space's brain took is `{"kind":"request",
  "text", "source", "space": "<space>"}`, and that brain's answer
  `{"kind":"answer", "text", "space": "<space>"}`. The main brain's carry
  no `space`; a request that fell back to it is kept as it was sent (`"In
  <space>: <text>"`, no `space`), which is what a space's view already
  reads as its own. A space brain's note turn is a `note` with its
  session's space (its own space when the note names no session).
- **Its words.** Each brain has its own turn (`brain::Turn`: watch, note,
  question), set as a note goes in and by its own transcript, which the
  narrator follows per brain (`~/.claude/projects/<its folder>`). A space
  brain's answer to a request is a chat message and is spoken like the main
  brain's; what it writes on a note turn is not shown or spoken at the
  quiet levels. Its Stop hook says whose words it brings (`Request::Say`
  has `brain`).
- **Decisions and receipts.** `tower raise` and `tower receipt` say which
  terminal ran them (`from`): what a space's brain raises carries its
  space whatever session it names, and its receipt without `--agenda` is
  its space's.
- **The radar** treats every `tower` and `tower-…` terminal on this Mac as
  a brain, not an agent: no watch, no question set, no decision, no alert,
  not in `agents`, not introduced, no task dispatched to it, no agenda
  task for what is queued to it. A session a space's brain starts is shown
  in a tab like one the main brain starts. The daemon routes to, restarts
  and stops only the brains it runs: the main one, and a `tower-…` whose
  folder is under its own `brains/` (one named by hand, or another
  tower's, is left alone). A test daemon (`HERDER_TOWER_TEST=1`) runs no
  space brains and touches none.

### What says so

- `missions[].tower` (in `state.json`, `tower missions`, the socket's
  `missions`): `{"name": "tower-sandbox", "status": "ready" | "working" |
  "starting" | "blocked" | "off", "address": "t12" | null}`, or `null` when
  the space has none and is due none (`per_space` off, no live session).
  `ready`: idle or done; `starting`: live, no status yet; `blocked`: it
  waits on its own screen or is in an error (its notes go to the main
  brain meanwhile); `off`: due one that doesn't run (it died and is waiting
  to be started again, or the main brain is down). It is never one of
  `missions[].sessions`. With no daemon, `tower missions` reads the brains
  from Herdr (then never `off`).
- `brains` in `state.json` (what `tower status` prints): every brain that
  runs, the main one first: `[{"name", "space": name or null, "address",
  "status": Herdr's word or "starting", "model": name or null, "effort":
  level or null}]` (`model` and `effort` as its command line says).
  `brain` and `brain_app` stay the main brain's.
- `{"cmd":"brains"}` on the socket: `{"brains": [as above]}`;
  `{"cmd":"brains","action":"close","space":"<space>"}`: `{"ended":
  [names]}`; `{"cmd":"brains","action":"restart"}`: `{"restarted":
  [names]}`.
- `tower up` starts the main brain; the daemon starts the spaces' own.
  `tower down` stops them all (`--keep-brain`: none). `tower restart`
  restarts the voice and leaves every brain running; `tower restart
  --brain` restarts the brains too. `tower doctor` lists them.

## The brains' model

One setting for every tower brain, the main one and each space's own:

```toml
[brains]
model = "claude-opus-5-5"   # the default when the key is absent
effort = "high"             # low | medium | high | xhigh | max; the default when absent
```

- The picker offers `claude-fable-5-2` (Fable 5.2), `claude-opus-5-5`
  (Opus 5.5), `claude-sonnet-5-5` (Sonnet 5.5) and `claude-haiku-5-5`
  (Haiku 5.5). The command line takes any other name the program does (an
  alias, a full name), as typed; one of the four may be given by its
  picker name ("opus 5.5").
- **How it is applied.** A brain is started as `claude -n <its name>
  --model <model> --effort <effort> <claude_args…>` (`brains::argv`):
  both hold for that session only. The tower never types `/model` or
  `/effort` into a brain (that would save the choice as the account's
  default), and sets neither `ANTHROPIC_MODEL` nor
  `CLAUDE_CODE_EFFORT_LEVEL`. So a change applies when a brain starts.
- **The user's own arguments win.** If `claude_args` contains `--model`
  (or `--model=…`), the tower passes no `--model` of its own; the same for
  `--effort`. The report says so (`locked_by_args`, `locked`).
- **Codex.** With `brain = "codex"` the model comes from `codex_model`,
  `codex_effort` and `codex_args`, as before: the report says so, setting a
  model is refused, and nothing is changed.
- **An install from before this.** The keys `claude_model` and
  `claude_effort` are read only from a file that has them and no `[brains]
  model` / `effort` (key by key); they are no longer written. A file with
  neither gets the defaults, so a brain that ran on the old built-in
  `sonnet` at `medium` starts on Opus 5.5 at `high` the next time it
  starts, and until then `restart_needed` is true.

### Commands and requests

- `tower model` prints the report; `tower model <model> [--effort
  <level>]` and `tower model --effort <level>` set it (config.toml's text
  is edited in place: only the `[brains]` keys change, and a change after
  which the file wouldn't read is refused); `--restart` restarts every
  brain that runs, now; `--json` (or a pipe) prints the report as JSON.
  It works with no daemon (then it restarts only the main brain, and the
  spaces' own come back on the setting when the voice starts).
- The socket: `{"cmd":"model"}` asks; `{"cmd":"model","model":"…",
  "effort":"…","restart":true}` sets (each field optional). The answer is
  the report with `"restarted": [names]`. The daemon does the restart
  itself (it outlives the brains, so the main brain can ask for its own):
  the spaces' brains are stopped and started by the next look, the main
  one is stopped and started on the program it ran.
- A restarted brain starts a new conversation.

### The report (`brain_model` in `state.json`)

```
"brain_model": {
  "program": "claude" | "codex",
  "model": "claude-opus-5-5",          // the setting
  "effort": "high",
  "choices": { "models": [ { "id": "claude-fable-5-2", "label": "Fable 5.2" }, … four ],
               "efforts": ["low", "medium", "high", "xhigh", "max"] },
  "running": [ { "brain": "tower", "space": null, "address": "t7",
                 "model": "sonnet" | null, "effort": "medium" | null,   // as its command line says
                 "restart": true } ],                                    // it was started on something else
  "restart_needed": true,              // any of `running` has `restart`
  "locked_by_args": false,             // claude_args set the model or the effort
  "locked": { "model": name or null, "effort": level or null },
  "says": "The tower's brain runs on sonnet at medium effort. Opus 5.5 at high effort applies when it restarts."
}
```

`running` has the main brain first, then the spaces' own by name. What a
brain "was started with" is read off its terminal's command line (`hc
get`'s `argv`), so it is true for a brain an older tower started too.
`restart` compares it with what a start would give now (the setting, or
what `claude_args` say). `says` is the sentence the Settings card and
`tower model` show.

### In Settings

"Tower's model" (`src/orb/panel/settings.rs`): the four models and the
five efforts as choices (`tower model <id>`, `tower model --effort
<level>`; the choice shows at once and goes back, with the command's error
in its place, if the tower refuses), `says` (with "A change applies when
the tower's brain restarts." when nothing else says so), and "Restart the
tower's brain" (`tower model --restart`) while `restart_needed`.

## Steps 4 and 6: the orb, the floating panel, the full window

Built in `src/orb/` (`model.rs` is the one model for both windows, `nav.rs`
where the full window is, `panel/` the views).

- **The orb** shows an amber number: open decisions outside muted spaces
  (O1 to O3). It replaces the red dot. No number when there are none.
- **Floating panel** (380 × 640, today's size; F1 to F9, I1 to I4): the
  status line, the queue (one card open, the rest behind it), the open
  spaces as chips with their state, the main tower's last reply, the input.
  Hovering the orb peeks at it; a click inside the peek pins it; a click on
  the orb closes it when it shows, and opens it pinned when it doesn't. It replaces the chat as
  the panel's home.
- **Full window** (960 × 640 at first; W1 to W20), opened from the floating
  panel in its place: the panel closes (after the click is handled: a
  window can't be closed from inside its own update). It is moved by its
  top, the header strip or the sidebar's top ("Tower" and the status
  line): a press becomes a native macOS move once the pointer travels a
  few points, so a click on a header button stays a click. It resizes by
  its edges, never under 820 × 520 nor over the screen. Where it is and its
  size are kept in `window.json` (the state folder, beside `orb.json`),
  moved wholly onto a screen (`nav::keep_on`), and it opens there next
  time (in the floating panel's place until it was ever moved). Opening
  it by choice (Full window, a space chip) is **full mode**, kept in
  `window.json` as `"mode": "full"`: the orb doesn't peek, and a click on
  it **minimizes** the full window into the orb when it is on screen and
  brings it back when it isn't. Minimized, the window is kept as it is,
  out of sight (`orderOut:`): the same page, scroll and typed draft come
  back; dictation isn't mirrored into it meanwhile. Esc and the round
  minimize button beside "Floating" minimize it too; it has no close
  button. **Floating**, beside "Tower", is back
  to the floating panel (`"mode": "floating"`): the full window closes and
  the panel opens, pinned. It has a
  sidebar (All spaces, each open space, New space, the quiet fold,
  Record, Shots, Settings) and a main pane:
  - All spaces: the queue and the missions list.
  - A space: its goal and done-when, Follow, Mute and **Review PRs** (`tower
    space review <space> on|off`; off, a line says its PRs get no
    reviewer; on, a line per open PR of the space with a review: "PR #12
    (shop) · Review: 2 critical, round 2 · fix-login-review-shop-12"), its closing report,
    what needs you there, its requests, its sessions (with worktree and
    port), the thread between its sessions (the latest five, "N earlier"
    for the rest the state holds: each with who wrote to whom and when,
    its colour by sender, its first four lines, and the whole of it with
    Copy on a click or "Show all"; the "From t7r (…)." it opens with is
    left out, the row names the sender), and an input that goes to that
    space's tower. Its head says "its own tower" when it has one that runs
    (`missions[].tower`; "is starting", "waits on its own screen" when so).
  - New space: the conversation that makes the space (drafts, changes,
    the planner), beside goal, project, what the tower will do (or the
    draft of a ticket), Launch (step 9); launched, the conversation is the
    space's, at the top of its own.
  - Projects: each project with its repos and their roles; add, edit, remove.
  - Record: the history, filtered by space (the old Chat and Activity).
  - Shots, Settings (how much it tells you, the tower's model, voice as a
    three-way choice "Off / Decisions / Replies and decisions" with
    "Always on this Mac" as its own switch, questions policy, Follow and
    Mute, behaviors, phone, accounts, projects), a session's own view.
- Number keys answer the open card. Cards are C1 to C6.
- One model feeds both sizes: answering or picking a card in one shows in
  the other.

### What the windows read

- `state.json`: `decisions` (in the daemon's order; the queue is those
  whose `state` is `open` or `answering`, the main queue those not
  `muted`), `missions` (`spaces`, `quiet`, `closed_today`), `threads`,
  `tell_level`, `questions` (the question set behind a `question` or
  `permission` decision, for its card's steps), `agents`, `screenshots`,
  `question_policy`, `behaviors`, `muted`, `tell_voice` (none on an older
  daemon: `muted` says off or replies), `voice_on_mac`, `talk_key`, `acts`
  (what the tower opened: the reply's receipt, its chips and whether Undo
  is offered), `brain_model` (the Settings card; none on an older daemon: the card says
  so).
  The orb's number is counted from `decisions`: those that are `open`,
  not muted, and not muted or dismissed here a moment ago. One whose
  answer is going in (`answering`) isn't counted, so for those seconds the
  number is one less than `decisions_count`.
- `spaces.json`: `follow` and `mute` (for a switch set before the state
  says so). The windows never write it.
- `prs.json`: a merge decision's PRs as the tower knows them (the commit a
  merge is pinned to, each check by name, its `review`). A PR that isn't in
  it is drawn from the decision's own `prs[]` (`title`, `checks`, `merge`,
  `review`). Each row of a merge card says where its PR's review stands
  ("Reviewing…", "Review: 2 critical, round 2", "Review: agreed"; amber
  until it is agreed), and asked to merge a PR whose review isn't agreed
  it says so among its warnings: the merge is still the user's.
- `history.jsonl`: the Record, and the reply to the user's last request
  (an `answer`, or a `receipt` that was told; never an entry with `record:
  true`), with the `act` entries that name that request (`act.request_at`):
  the act is shown as `state.json` has it now, or, when the tower no longer
  holds it, as its last entry left it (then with no Undo).
- A state without `decisions` or `missions` (an older daemon) is read
  through `model::adapt` and `adapt_missions`: its escalated question sets,
  the waiting PRs of `prs.json`, `spaces.json` and Herdr's spaces.

### What a decision's `state` means to a card

`open`: a card. `answering`: a short "Sending your answer" card, not
counted. `answered`, `stopped`, `dismissed`: gone (the state lists open
ones only). `stopped` is over without an answer: its sessions are gone. An
answer that stops half way (the dialog changed while it was typed) leaves
the decision `open` again; the card then says "It stopped", from its
question set's `status: failed`, and offers the session or another try.

### What they do

- A card's answer goes through the daemon's socket, as `tower answer`
  does: `{"choice": n}` or `{"other": "…"}` for what was raised or failed,
  the question set's answers for a question. A decision of the daemon's
  (`d…`) is answered and dismissed by its own id, so every session that
  asks the same gets the answer; an older daemon's by its question set's.
- A merge runs in the window's process (`prs::merge`, pinned to the commit
  shown), then tells the daemon (`PrMerged`).
- What the full window sets runs the `tower` CLI: `tower tell <level>`;
  `tower space follow|unfollow|mute|unmute <space>`; `tower space review
  <space> on|off`; `tower space goal
  <space> "<goal>" --done-when "<…>"` then `tower space standing <space>
  on|off`; `tower space close <space>` (and `--force` after a refusal),
  whose JSON receipt is said in a sentence; `tower setups --json`; `tower
  setup add|remove …` (Projects); `tower launch "<goal>" --dry-run --json`
  for the preview and the same without `--dry-run` to launch (its receipt
  names the space, whose view then opens); `tower launch "<ticket>"
  --draft --json` (and `--adjust … --prior …`) for a draft, and `tower
  launch "<ticket>" --repo … --name … --title … --done-when … --review|--no-review --json` to
  launch it (`--review` or `--no-review` on a plain launch too, as the
  switch stands).
- The input in a space's view runs `tower ask --space <space> "<text>"`
  (step 7): the daemon hands it to that space's brain, or to the main one
  as `In <space>: <text>` when the space has none that can take it. The
  placeholder says which ("Tell the <space> tower…" when
  `missions[].tower` says one runs, else "Tell the tower about
  <space>…"). On a tower whose `ask` has no `--space` (an older build) the
  window sends `tower ask "In <space>: <text>"` itself. Either way the
  reply shows in that space's view: an `answer` with that `space`, or the
  answer to a request that starts with `In <space>:`.
- "Tower's model" in Settings runs `tower model <id>`, `tower model
  --effort <level>` and `tower model --restart` (see "The brains' model").
- Voice in Settings runs `tower tell --voice off|decisions|replies`; the
  panel's Mute and ⌥-click on the orb stay `tower mute`.
- Undo on a reply runs `tower undo <id> --json` (and `--force` from "Undo
  anyway"); "Make it a rule" on a receipt runs `tower ask [--space <space>]
  "Make this a rule: …"`.

### Looking at them without the keyboard

`tower orb --ui-dir <dir>` with `HERDER_TOWER_ORB_PANEL=show[@<x>,<y>]`
(the floating panel up from the start) and
`HERDER_TOWER_ORB_WINDOW=show[:<view>][@<x>,<y>]` (the full window at
`all`, `space=<name>`, `new[=<goal>]`, `record[=<space>]`, `shots`,
`settings`, `behaviors`, `phone`, `session=<space>/<address>`). Neither
takes the keyboard; a spot off the screen's edge keeps them out of the way.
`HERDER_TOWER_ORB_ANSWER=<decision id>:<n>[:rule]` has the full window
answer that card with its option `n` (from 0) two and a half seconds after
the start, as a click on it would, and with `:rule` take its receipt's
"Make it a rule": for looking at what an answer leaves behind.
`HERDER_TOWER_ORB_KEYS=<card id>:<keys>` opens that card's field for its own
words and types `keys` into it as key presses, through the window's own key
handling (the window needn't be in front): text runs, and `{enter}`,
`{shift-enter}` or another key in braces; what the field holds and whether
the answer went is logged after each run. On a merge
card `n` is its button (0 asks about the first PR, which shows what
merging it does and its warnings; nothing merges before Confirm).
A whole tower for them to read runs in a sandbox with `HERDER_TOWER_VOICE=off
tower daemon` (no speakers, no Kokoro, no Whisper, no microphone, no talk
key; its own `HERDER_TOWER_HOME`, `_STATE`, `_DATA`, and stand-ins named by
`HERDER_TOWER_HC`, `HERDER_TOWER_HERDR_APP`, `TOWER_CCA`, `HERDER_TOWER_GH`).

### Enter sends

Every text box of the floating panel and the full window takes Enter as
its card's or form's own button, and Shift+Enter as a new line: "Tell the
tower" (the conversation columns and New space's conversation are the
same box), a card's own words ("Answer"; on a question card its Answer,
Next or Review, not while its answer goes in), the space's goal editor
(Save), a behavior's editor (Save), a project's form (Save, a repo per
line with Shift+Enter) and the phone's name. Search behaviors takes no
new line.

### What is typed and not sent is kept

Each input keeps its own unsent text (`crate::unsent`, in
`<data>/drafts.json`, `~/.local/share/herder-tower/drafts.json`), across a
restart of the tower or the orb, a window closed and opened again, and
switching views. It goes once it is sent (or emptied); one left alone for
30 days is let go. The inputs, by key:

- `main`: "Tell the tower" in the floating panel and in All spaces (both go
  to the main tower);
- `space:<name>`: a space's conversation; `session:<address>`: a
  session's view; `behaviors`: the behaviors' input;
- `new:<draft id>`: a new space in draft in New space, once something was
  said in it (it is kept then); `new:fresh` before that, since a window
  opened again starts the fresh one under another id. What was typed for
  a draft and not said goes to the new space's own conversation when it
  is launched, and goes with a draft that is deleted;
- `other:<card>:<question>`: a card's "Other" field (its closed pill shows
  the kept words; a question's tab with words has "Other" chosen again).
  All of a card's go once its answer went.

The message field is one box: it shows the text of the view showing
(`typed::input_key`, swapped in as the view or the new space in draft
changes, the caret at its end) and writes what it holds on each change.
Files dropped on it are kept with their places (`[Image #1]` comes back as
a token). A take the voice writes into an empty field, and a take's words
on their way, are not kept.

### Answers paired with their questions

A question and its answer are paired in the history (`Entry::reply_to`:
the answered message's `at`), so the conversation shows which answers
which, whatever came between them and in whatever order they came back:

- **The tower's answer** names the request its turn began with: the
  brain's Stop hook has the turn's prompt (`[voice] <the words> …`), and
  `history::request_for_prompt` finds the user's request to that brain
  (the main one, or a space's own) whose words open it. A note's or a
  watch's turn answers none.
- **A session's reply** is kept as one (kind `reply`): when a session's
  turn ends, what started it (`history::message_for_turn`: its prompt,
  matched to the user's words in its terminal, their answer on its card,
  or the first line of what the tower queued for it, from the 30 minutes
  before) is what it answers, once. A turn a message from another session
  or a note started answers none.
- **In the conversation column** (a space's, the main tower's, a
  session's thread): an answer has a line on top, "↳ you asked: …" (or "↳
  the tower sent it: …"), that goes to the question; an answered question
  goes to its answer when it is clicked anywhere (or its "answered ↓").
  What it went to comes to the top of the column and lights up a moment
  (a cyan outline, fading over 1.8 s: `FullUi::chat_flash`). A session's
  reply is a row of its own, headed by its name.
- **While the tower hasn't answered** a question of yours to it, the
  question has a spinner, "waiting for the tower · 12s", and **Take
  back**. That spinner clears as the answer comes in. A session's
  question says "waiting for an answer" (for 12 hours). When no answer
  will come, the question says why, in amber, instead of waiting:
  - the brain's turn ended with nothing to say: "The tower's turn ended
    without an answer." (its Stop hook);
  - it failed: "The tower couldn't answer: it hit its rate limit." and
    the like (the brains' **StopFailure** hook, `tower hook
    claude-failed`, by its `error`). At its Claude account's usage limit
    it isn't left unanswered: the tower moves that brain to the next
    account with usage, and gives it the same prompt again there (see
    the README, "When an account hits its usage limit");
  - you took it back: "You took it back." (**Take back** sends
    `{"cmd":"withdraw","at":…}`; the daemon stops the brain with `hc
    interrupt` if that request is still the latest it was given and it is
    working);
  - the main brain isn't running: "No answer: the tower isn't running.";
  - half an hour passed: "No answer came."

  The first three are kept in the history as `unanswered` entries
  (`reply_to`: the request; `status`: `empty`, `failed` or `taken-back`;
  always `record`), so they survive restarts. A question isn't claimed
  either way when an answer after it names no question (its link failed),
  or when it is older than half an hour and was asked before answers were
  paired. (`talk::standing` decides.)
- **On the phone**: `GET /v1/chat` has `reply_to`, `asks` and
  `answered_by` (see the phone API's "Answers paired with their questions").
  It doesn't have the pending or no-answer state yet. The app would show a
  pending indicator on a message of the user's without `answered_by`, but
  it can't tell "still coming" from "never coming". That needs the
  `unanswered` entries served as a field (say `unanswered: {why, text}`) and
  a Take back call (`POST /v1/chat/{id}/withdraw`), plus tap-to-scroll on
  `asks.id` and `answered_by`, which the app can do already.

## Step 11: the phone

The Android app (its own repo, `~/Work/herder-tower-android`) opens on the
queue and notifies for a decision and for nothing else (A1 to A12; picture
in `docs/quiet-tower-android.png`). The phone API has what it needs, all
under `/v1`, authenticated as before. **The contract the app is built
against is [mobile-api.md](mobile-api.md), "The Quiet Tower"** (every
shape, error and event), with a walk through every call in
[mobile-api-quiet-walkthrough.md](mobile-api-quiet-walkthrough.md). This
section says what was built and how it hangs together.

### The calls

| Call | Body | Answer |
|---|---|---|
| `GET /v1/decisions?space=<name>&all=1` | | `{"space", "count", "decisions": [Decision]}` in queue order (`tower decisions --json`). Without `space`, muted spaces are left out; with it, that space's only, muted or not. `all=1`: `closed` too |
| `GET /v1/decisions/{id}` | | `{"decision": Decision}`, open or closed lately |
| `POST /v1/decisions/{id}/answer` | `{"choice": n}`, `{"other": "text"}`, or what `/v1/questions/{id}/answer` takes (`{answers, submit}`; as a form with files too) | What `tower answer <id>` prints, by kind (step 3). `409 closed` (with `state`) when it is over already; `400 merge` (with `prs`) for a merge |
| `DELETE /v1/decisions/{id}` | | `{"dismissed": id, "state"}` |
| `POST /v1/decisions/{id}/interpret` · `/voice?tab=N` | `{"tab", "text"}` · a WAV | A spoken answer as a choice: a question's as `/v1/questions/{id}/interpret`; for the others their options are the one tab, and `answer` is the body to send |
| `GET /v1/missions` | | `{"spaces": [Mission], "quiet": [name], "closed_today": [name]}` |
| `POST /v1/spaces/{space}/goal` | `{"goal", "done_when", "standing"}`, each optional | What `tower space goal` prints, with `mission` (its row of the missions, or null) |
| `POST /v1/spaces/{space}/follow` · `/mute` | `{"on": bool}` | `{"space", "follow"\|"mute", "mission"}` |
| `POST /v1/spaces/{space}/close` | `{"force": bool}` | `202 {"action": Action}`; `409 busy` (with `sessions`) while a session works or is blocked, unless `force` |
| `GET /v1/thread?space=<name>&limit=50&before=<id>` | | `{"space", "messages": [{"id","at","from","from_name","to","to_name","text","space","delivered"}], "more"}`, newest last |
| `GET /v1/setups` | | `{"setups": [{"name","account","about","repos","roles"}]}` |
| `POST /v1/launch` | `{"goal", "setup", "name", "dry_run"}` (`goal` required) | `dry_run`: `200`, the plan (`tower launch --dry-run --json`). Else `202 {"action": Action}` |
| `GET /v1/actions` · `/v1/actions/{id}` | | `{"actions": [Action]}` · `{"action": Action}` |
| `GET /v1/tell` · `POST /v1/tell` | `{"level", "voice"}`, each optional | `{"level", "voice"}` · the same with `"was"` and `"voice_was"` for what was set. `400 bad_level`, `400 bad_voice` |
| `GET /v1/acts` · `/v1/acts/{id}` | | `{"acts": [Act]}`, newest first · `{"act": Act}`; `404 no_act` |
| `POST /v1/acts/{id}/undo` | `{"force": bool}` | `200` what `tower undo` prints (`act`, `closing`, …). `409 held` (with `sessions`: who and why) while work would be lost, unless `force`; `409 undone`; `409 cannot_undo`; `404 no_act` |
| `POST /v1/decisions/{id}/rule` | `{"answer": text}`, optional | `202 {"sent": the request, "space"}`: asked of that space's tower as `/v1/ask` asks. `409 not_answered`, `409 no_rule`, `404 no_decision` |
| `GET /v1/model` · `POST /v1/model` | `{"model", "effort", "restart"}`, each optional | The report under "The brains' model", with `restarted` |
| `POST /v1/ask` | gains `"space": name` (a `space` field in the form) | `202 {"sent", "attachments", "space"}`; `404 no_space` |
| `POST /v1/voice` | gains `?space=<name>` | Said in that space's view, a take goes to that space's tower, as `/v1/ask` with `space`: `200 {"heard", "sent", "attachments", "space"}`; `404 no_space` before anything is transcribed or saved. Dictation (`send=0`) takes no space |
| `GET /v1/chat` | gains `?space=<name>` and `?main=1` | Every message has `space`; a space's conversation pages back with `before` as the main chat does, its acts in it |
| `GET /v1/history` | gains `?space=<name>` | A space's Record |

`GET /v1/hello` (and the pairing answer, and the stream's `hello`) has
`"features": ["quiet", "acts", "pairing", "bonjour"]`: an app checks `quiet` before it uses any
of this, and falls back to the question sets and PR chips on a tower
without it; `acts` says the tower keeps acts (the chat's `act` messages,
`/v1/acts`, the undo, `/v1/decisions/{id}/rule`, `voice` in `/v1/tell`);
`pairing` says a phone pairs again in place and learns the tower's new
addresses while connected (mobile-api.md, "Pairing on more than one
network"); `bonjour` says the Mac is found by name on whatever network
it is on (`_herdr-tower._tcp`, with the certificate's fingerprint as `fp`
in the TXT): a paired phone that reaches none of the addresses it knows
browses for it, takes the one whose `fp` it pinned, and connects with the
same pin and token, so one pairing works at home and at the office
(mobile-api.md, "Finding the Mac by Bonjour"). `api` stays 1: nothing that
was there changed.

```
Decision { "id", "key", "kind": "permission"|"question"|"merge"|"choice"|"failed"|"raised"|"blocked",
           "space": name or null,
           "sessions":[{"address","name","waiting":bool,"question_id"?}], "waiting": n,
           "title", "detail", "why", "options":[{"label","why","pick":bool}], "pick": label or null,
           "prs":[{"key","repo","number","base","title","url",
                   "checks":"pass"|"pending"|"fail"|"none","note","session",
                   "merge":{"can","blocked","needs_override","warnings"}}],
           "question_id": id or null, "task": id or null,
           "first_ms", "last_ms", "count",
           "state": "open"|"answering"|"answered"|"stopped"|"dismissed",
           "muted": bool }
Mission  { "name", "goal", "done_when", "standing", "state": "open"|"quiet"|"ready",
           "follow", "mute", "needs": n, "report",
           "sessions":[{"address","name","status","account","worktree","branch","port","said"}],
           "requests":[{"id","title","state","done","total"}], "thread_count",
           "tower": {"name","status","address"} or null,
           "quiet_since_ms", "ready_since_ms", "auto_close_ms" }
Action   { "id": "act_" + 8 hex digits, "kind": "launch"|"close", "space",
           "state": "running"|"done"|"failed",
           "plan": a launch's dry-run receipt (null for a close),
           "receipt": a launch's receipt so far, then whole; a close's when done; else null,
           "error": why it failed, or null,
           "by": "phone:<device>", "started_ms", "finished_ms" }
```

The exact `Mission` (what is null, what is "", the order of the list) is
under "`missions` in `state.json`" above. A closed space is not in the
list: its name is in `closed_today` on the day it closed.

`prs` is `[]` for anything but a merge. `checks` is `none` when no checks
ran; `note` says the checks in words ("1 check failing: lint"); `merge` is
what a PR's chip has (`prs::Gate`). `why` is the reason a question went to
the user, `merge`, the category `tower raise` gave, or `failed`. `task` is
a failed decision's agenda task. A merge is never answered by its
decision: the app goes to the PR's own `/v1/prs/{owner}/{repo}/{number}/confirm`
and `/merge`, by `prs[].repo` and `prs[].number`.

### One way in

The phone does what the Mac's windows and the CLI do, through the same
functions (`src/daemon/lan/tower.rs`: the trait `Tower`; `src/daemon/desk.rs`:
the daemon is one):

| Phone | The same as |
|---|---|
| `/v1/decisions`, `/v1/decisions/{id}` | `{"cmd":"decisions"}` (`Decisions::listed`: a space by its whole name, where the CLI's `--space` takes a part) |
| `…/answer`, `DELETE` | `{"cmd":"answer"}`, `{"cmd":"dismiss"}` with a decision's id: `Decisions::answer_with` and `dismiss_with` (`src/daemon/decisions.rs`), carried out with the daemon's question sets, send queue and agenda ledger. They return `Refused` (unknown, closed, a merge, a bad answer, its questions' own errors, failed): the socket prints it as a sentence, the phone gives each its status and code |
| `/v1/missions` | `{"cmd":"missions"}` (`Missions::now`: it looks first) |
| `…/goal` | `tower space goal --by you` and `tower space standing`: `missions::goal_by`, `standing_in`, under `spaces::update` |
| `…/follow`, `…/mute` | `tower space follow\|mute`: `spaces::set_follow` / `set_mute` under `spaces::update`; a mute republishes the queue; the missions are looked at again for the answer |
| `…/close` | `tower space close`: `missions::holding` for the refusal, `missions::close(space, force, false)` (the voice closes it itself), then its decisions and the missions follow |
| `/v1/setups`, `/v1/launch` | `tower setups`, `tower launch`: `setups::value`, `launch::planned`, `launch::open` (which now reports after each session) |
| `/v1/tell`, `/v1/model` | `{"cmd":"tell"}`, `{"cmd":"model"}` |
| `/v1/acts`, `…/undo` | `{"cmd":"act","action":"list"}`, `{"cmd":"act","action":"undo"}`: `Acts::list`, `Daemon::undo` (`acts::undo` with the daemon's queue, ledger, watches and missions) |
| `/v1/decisions/{id}/rule` | "Make it a rule" on the Mac: `Decisions::rule_of` (`decisions::rule_for`), then the same `ask` |
| `/v1/ask` with `space` | `{"cmd":"ask","space"}`: `stt::deliver_to` |

The phone API starts when the daemon is whole (it is handed the daemon,
`Lan::attach`), a moment later than before; until then these calls would
answer `503 starting`.

### What takes a while is an action

A launch opens a session per repo (`tower start` each, up to three minutes
before one is given up), a close ends terminals and removes worktrees.
Neither holds a phone's call open, and neither blocks the listener (every
call has its own thread, and the work its own after the call has
answered). `POST /v1/launch` makes the plan at once (so a setup nobody has,
or a repo that is no folder, is the call's own error), answers `202` with
an action that carries it, and opens that very plan (it is not planned a
second time: what the action shows is what is opened). `POST /v1/spaces/{space}/close`
checks at once that no session there works or waits (`409 busy`, naming
them), then answers `202` with an action. Every step is an `action` event
(begun; a launch's receipt so far after each session; done or failed), and
`GET /v1/actions/{id}` says the same to a phone that was away. One launch
at a time, and one close per space (`409 launching` / `closing`, with the
running action's id, so a second tap finds the first). Actions live in the
daemon's memory (the last 50): a restart forgets them, and what they did
is in the missions.

### The state, and the events

The phone's state (`GET /v1/state`, the `state` event) has
`decisions_count`, `tell_level`, `tell_voice`, `brain_model` and `brains`,
and none of the lists: `decisions`, `missions`, `threads` and `acts` are
taken out of it
(`state.json` on the Mac keeps them all). A `state` event is sent only when
something the phone's state carries changed (its `version` and
`updated_ms` aside), so a change to the queue or a mission alone doesn't
send it.

The event stream gains:

- `decisions`: `{"count", "decisions": [Decision]}`, every open one in
  queue order with `muted` (the main queue is those not muted; `count` is
  `decisions_count`). Sent right after the first `state`, then whenever
  the list changes, the 30-second reorder included.
- `missions`: what `GET /v1/missions` answers, after the first
  `decisions`, then whenever it changes.
- `thread`: `{"space", "message"}` when a `peer` entry is written to the
  history (right after that entry's `history` event).
- `action`: an action, at each step.
- `act`: an act (the shape under "A command gets a receipt"), when it is
  kept, when a session joins it, when it can no longer be undone lightly,
  and when it is undone.

**The chat.** What the tower opened in answer to a request is one message
of the chat, `role: "tower"`, `kind: "act"`, where it was opened (so
between the request and the tower's own words): `text` is the act's line
(its closing line once undone), `act` the act as it is now, `space` the
conversation it was asked in. It changes in place (a `chat` event, `op:
"update"`). An act the tower no longer holds (older than its last 50)
shows as its history entry left it, with `undo.possible: false`. The
answer to `POST /v1/decisions/{id}/answer` has `rule` (`{"can", "text"}`).

**New, or changed.** The app notifies for a new decision and for nothing
else, from the `decisions` event alone: it keeps the ids it has seen, and a
decision whose id it has not seen, that is not `muted` and is `open`, is
new. An id is a decision's for its whole life; the same key again (another
session, the same line raised again, another PR of the repo) changes
`count`, `last_ms`, `sessions` or `prs`, never the id; the same thing
needing the user again after it closed is another decision, with another
id. A muted space's decision is seen and never notified.

### Whose conversation

A request asked in a space's view and the answer to it belong to that
space's conversation, on the Mac and on the phone. Every chat message has
`space` (`chat::thread_with`): a request's is the space it was asked in
(its history entry's `space` when the space's own tower took it; the space
named in front, `In <space>: …`, when the main tower took it for a space it
keeps); an answer's is its request's (the main tower's answers are matched
to its requests in order); for anything else it is where it comes from.
`chat::in_conversation` is the one rule for both filters: a space's own
conversation is its requests, answers, receipts and acts; the main one is
everything but the requests, answers and acts that have a space. An act's
`space` is its request's conversation: the space whose own tower took the
request, or, for one the main tower took as `In <space>: …`, that space (by
the request it answers, `request_at`).

### Tests

`cargo test lan` runs the listener over HTTPS with a pinned certificate and
a paired phone, against a tower of the test's own (real decisions and
question sets, a `spaces.json` in a temporary folder, a launch that goes
when the test says): every call's answer and its errors, names with spaces,
slashes and letters outside ASCII, the events for a new decision and for a
repeat, a launch step by step, and the calls from before still answering.

## Reference

- The design's source: `docs/quiet-tower.html` (open it in a browser). Its
  script holds the mock's model and drawing code, which says exactly what
  each view shows.
- The numbers above: `~/.local/share/herder-tower/history.jsonl`,
  `agendas.json` and the brain's transcripts for 10 Nov 2026.
