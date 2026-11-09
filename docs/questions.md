# Questions: what sessions ask, answered for you or with a tap

Sessions ask in two ways, and both become the same **question set**:

- **A dialog** (`kind` `claude-ask`, `claude-plan`, `claude-permission`,
  `codex-menu`): the session is blocked on a terminal menu.
- **Plain text** (`kind` `text`): the session finished its turn with a
  message that asks the user something ("Want me to open the PR, or wait?").

The tower's brain recommends three answers per question and decides, by the
**90/10 rule**: it answers routine ones itself (about 90%) and leaves the
rest to the user (see "Who decides").

When a Claude Code or Codex session stops to ask something in a dialog, it shows a
terminal dialog: Claude Code's tabbed questions (`AskUserQuestion`, one tab
per question, options, "Type something", a review tab and Submit), its
plan approval, its permission prompts, Codex's approval menu. The tower
reads that dialog into a **question set**, shows it in the Mac panel and the
Android app, gets three recommended answers per tab from its brain, and
types the user's answers into the dialog for them, step by step, checking
the screen after every step.

Code: `src/questions.rs` (reading, the data, answering), `src/daemon/asks.rs`
(the open sets, the brain, notices). Fixtures: `tests/fixtures/questions/`
(real screens and a transcript, captured from live sessions).

## Reading

The radar sees a session turn `blocked` (herdr-core's status, with `kind`
`question` or `permission` and a one-line `msg`). It reads the session's
screen (`hc read`) and finds the dialog at the bottom:

| Dialog | Recognised by | Kind |
|---|---|---|
| Claude Code questions | the tab bar `←  ☐ Database  ☒ Features  ✔ Submit  →` (one question: ` ☐ Color`) | `claude-ask` |
| … its review tab | `Review your answers` | `claude-ask`, `submit_state: review` |
| Claude Code permission | `Do you want to proceed?` | `claude-permission` |
| Claude Code plan | `Would you like to proceed?` under the plan | `claude-plan` |
| Codex approval | `Would you like to run the following command?` | `codex-menu` |

Options are the numbered lines (`❯ 1. Postgres`), their descriptions the
indented lines under them; multi-choice options have `[ ]` / `[✔]`. For
Claude's questions the screen shows only the current tab, so the tower also
reads the session's **transcript** (`~/.claude/sessions/<pid>.json` names it;
the account's config folder for other accounts): the last `AskUserQuestion`
call without an answer has every question, option and the multi-choice flag
exactly (`exact: true`). Without it, the other tabs are known only by title
until visited.

A screen it can't read gives no question set: the user answers as before,
through the tower ("approve it", "say no").

Every change of herdr-core's status line (a new tab, a new question) reads
the screen again; the set's `id` (a hash of the session, kind, title, detail
and questions: another command or plan is another set) stays the same while
the questions do, so recommendations stay with it. When the session stops
being blocked, its set closes, and the same dialog asked again later (the
next "Allow Bash: npm test?") is a new question: told to the brain again,
with its own 40-second timer. A dialog the user dismissed stays away while
the session still shows it.

## The question set

```json
{
  "id": "q2bd2c2bc9b6a", "session": "t22", "name": "Claude in eyzflow",
  "kind": "claude-ask",            // claude-ask | claude-plan | claude-permission | codex-menu
  "title": "3 questions: Database, Features, Deploy",
  "detail": "",                    // the command, the plan
  "context": "Set up the new app", // what the session was asked to do (its open job)
  "tabs": [
    {"title": "Database", "question": "Which database should the app use?",
     "options": [{"label": "Postgres", "description": "…", "key": "1"}, …],
     "multi": false, "allows_other": true, "answered": false},
    …
  ],
  "current_tab": 0,                // tabs.len(): the review tab
  "submit_state": "answering",     // answering | review | single (no submit step)
  "recommendations": null,         // null while the brain thinks; then [{tab, options: [{label, why, choice}]}]
  "status": "open",                // open | answering | answered | failed
  "error": null,                   // why answering stopped
  "exact": true,
  "at_ms": 1794171202515
}
```

It is in the daemon's `state.json` (`questions`, and each agent's
`question` id), in the phone API, and pushed as a `question` event.

## Plain-text questions

When a session goes from `working` to `done` or `idle`, the tower reads its
last message (from its transcript for Claude Code on this Mac, else from its
screen without the interface around it) and checks the end cheaply: a `?`
near the end (not in code or a link), "want me to", "should I", "which do you
prefer", "let me know", "soll ich", … or options laid out to choose from.
Only then does it open a `text` question set (one tab, its guess at the
question, `allows_other: true`) and send the brain
`[question-text] <session> <id> <json>` (the message's end, what the session
was asked to do). The brain says what it really asks (one or more
questions, each with three replies) or `tower recommend <id> --none`, which
closes it. A text question set closes by itself when the session starts
working again (it got a prompt from anywhere), and can be dismissed
(`DELETE /v1/questions/{id}`, `tower dismiss <id>`).

Its answer is **one reply** typed into the session as the user would (no
sender note): the chosen recommendation's text or the user's own words
(typed or spoken, with files attached as the panel does), several questions
numbered `1.`, `2.`, …. If the session is working again by then, the reply
waits (`status: queued`) and goes in when that turn ends. A waiting reply
is dropped (and logged) when the session is gone, after 30 minutes, or when
the session finished and started a turn of its own meanwhile (herdr-core's
turn count; a dialog on the way doesn't count). It is typed only while the
session is idle or done (read again just before; `hc send --if idle` when
idle), never into a dialog.

## Waiting for you

A turn can also end handing something to the user without asking a
question: "PR #1250 is open; the merge command is blocked for me, so you need
to merge it. I'll wait." (`questions::waits_for_user`, checked before the
question test). In its last few sentences, one addressed to the user ("you",
"the user") that names something to do (merge, approve, review, test,
decide, log in, install …) and asks or waits for it ("please", "must",
"need to", "once you", "waiting for"), or a plain "waiting for your …",
"over to you", "bitte …". Links are left out of it.

It needs the user as a blocked session does:

- a `text` question set whose question is that sentence, `decision:
  escalate` from the start and `reason` "it waits for you to do this": the
  brain is not sent `[question-text]` (it can't merge or test for them, and
  mustn't answer for them). The user replies from the card ("merged, go
  on") as to any text set;
- the session's open job goes to `blocked`, with the request as its text;
- it is said: "<name> waits for you: <request>", by the tower; from a space
  out of focus, as "In <space>: …". When the brain watches that session, the
  watch's report says it instead (`[watch] … It is waiting for the user to
  act: "…"`), marked blocked, and the job is blocked rather than done.

## Recommendations

When a new set appears, the daemon sends the tower's brain one line:
`[question] <session> <id> <json>` (the tabs with option indexes, and what
the session was asked to do). The brain answers only with

```sh
tower recommend <id> --json '[{"tab":0,"options":[{"label":"SQLite","why":"one file, enough for now","choice":1}, …]}, …]'
```

three per tab, each a valid choice (checked: an option index, `{"options":[…]}`
for multi, or `{"other":"text"}`). The set shows at once with
`recommendations: null`; they land with an `update` when ready. The brain's
turn for a `[question]` is never spoken and never kept in the chat (its
prompt rule says to stay quiet, and the voice drops what it writes then).

## Who decides: the 90/10 rule

With its recommendations the brain gives a decision: `tower recommend <id>
--decision auto|escalate --reason "…"`.

- **auto**: the tower answers right away with recommendation 1 for every
  question (it is always the default pick), once, and writes a history entry
  (`kind: answered`): "answered for you: Which test runner? -> Vitest,
  because …". It shows in the chat as a small event and is never spoken.
  On a menu (permission, plan, Codex) it only ever gives a plain one-time
  yes (`questions::safe_auto`): an option that would stop future prompts
  ("Yes, and use auto mode", "Yes, and always allow access to /tmp",
  "Yes, and don't ask again for …", "allow all edits during this session",
  bypass permissions, auto-accept edits) is swapped for the plain yes ("Yes",
  "Yes, proceed", "Yes, manually approve edits"), and the swap is logged; with
  no plain yes, or any other pick ("No", "Tell Claude what to change"), it
  escalates. This holds for every policy.
- **escalate**: the card stays for the user, who hears one sentence: "The
  web session asks: Should I deploy to production? I'd say: Not yet." (held
  for later if another space is in focus, as notices are).

The brain's prompt has both lists (routine: naming, which of two reasonable
implementations, proceed, run the tests, edits and commands inside the
project, lint, local commits, the safe default; for the user: deploys,
pushes, merges to main or prod, deleting or overwriting, infrastructure,
secrets, money, outside the project, security, big scope or product
decisions, anything surprising or uncertain). The daemon enforces the hard
part whatever the brain says (`questions::must_escalate`, on the title, the
detail, which holds a prompt's command, the context and the options, words
matched whole so "prompt" isn't "prod" and `pushd` isn't a push): a question
about deploying or releasing (`npm publish`, `cargo publish`; not `cargo
build --release` or release notes), pushing (a bare "push?") or merging (a PR,
`gh pr merge`), deleting or overwriting (`rm`, removing files or data, `git
clean`, `git checkout -- .`, `git reset`, `git restore`, `docker rm`, `docker
system prune`, `DROP`, `DELETE FROM`), rewriting git history (`git rebase`),
running as root or changing permissions (`sudo`, `chmod -R`, `chown`), killing
processes (`kill -9`, `pkill`), skipping the checks (`--no-verify`),
infrastructure, secrets or money always escalates. Right before an answer is
typed for the user the dialog is read again, and the policy, this rule and
the menu check run on what it shows now; if one fails, it escalates instead.

The user's policy (`tower policy`, `PUT /v1/questions-policy`, kept in
config.toml as `question_policy`): `normal`; `keep-moving` (the brain answers
borderline ones too, never the hard list); `ask-everything` (nothing is
answered for the user).

If the brain gives no decision within 40 seconds (it's away or busy), a
dialog escalates by itself. A dialog the tower can't read gets the plain
notice, as before.

## Answering

`POST /v1/questions/{id}/answer` (phone), the panel through the daemon
(`Request::Answer`), or `tower answer <id> --json '[…]'`:

```json
{"answers": [
   {"tab": 0, "choice": 1},
   {"tab": 1, "choice": {"options": [0, 2], "other": "Exports, as CSV"}},
   {"tab": 2, "choice": {"other": "Railway, with a staging app\nand preview deploys"}, "attachments": ["/…/attachments/…/1-sketch.png"]}
 ],
 "submit": true}
```

`choice`: an option's index (0-based), `{"options": [...]}` (multi-choice),
`{"other": "text"}` (a typed answer, several lines allowed), both on a
multi-choice tab, or `{"spoken": "the second one"}`. Files for a typed answer
are attached exactly as the panel does: `[Image #1] text [attached: path]`.

The daemon reads the dialog first (nothing is typed if the questions changed
since they were shown), then, for each tab, checking the screen after every
key:

1. Goes to the tab (Right/Left, compared by its question text), first
   stepping off typed text if the cursor is in it (Up).
2. Single choice: the option's number (Claude picks it and moves on; if it
   only moved the cursor, Enter). A typed answer: the number of "Type
   something", then the text is pasted (with Enter).
3. Multi-choice: each option's number until exactly the wanted ones are
   ticked; a typed answer: down to its row, paste, Up off it, its number to
   tick it.
4. Submit: to the review tab; it must not warn that a question is
   unanswered, and must show every answer given; then "Submit answers".
   Never blind: anything else stops it.
5. Menus (permission, plan, Codex): the option's number, and Enter if the
   number only moved the cursor; done when the prompt is gone.

On any surprise it stops and says where (`status: failed`, `error`), and
the user can finish in the terminal. Text is typed with `hc send` without
the tower's terminal identity, so it goes in as it is (from a terminal hc
would wrap it in a note saying who sent it), with `--` before the terminal
and the text (a reply like "- yes" isn't a flag), and without control
characters (C0, DEL, C1; line breaks and tabs stay).

One answer at a time: a set is marked `answering` in the same step that
checks it isn't already, so the phone, the panel and the tower can't type
into one dialog together, and keys for a session go in under its typing
guard (`hc::typing(address)`), held while a dialog is driven or a reply is
typed.

## By voice

The phone records a take and posts it to
`POST /v1/questions/{id}/voice?tab=N` (or transcribes with
`/v1/transcribe` and posts the text to `/v1/questions/{id}/interpret`): the
answer is `{"heard": "...", "choice": {"options": [1], "other": null}}`.
"The second one", "zweite", "number 3", "the last one" or an option's name
pick options ("first and third" on a multi-choice tab); anything else is a
typed answer. The app shows it and the user confirms (it isn't sent by
itself).

## Notices

Questions go through the brain: the user hears only escalated ones, in one
sentence with the tower's pick. Other blocked sessions (and dialogs the tower
can't read) get the notice from herdr-core's status line, in a sentence: "The orb session asks: Which
database should the app use?", "… asks to run touch /tmp/x", "… has a plan
ready for you to approve", "… asks to edit login.rs"; just "… needs you"
when there is nothing to say. Focus still holds news from other spaces.

## What's reliable, and what isn't yet

- Tried live, end to end (the driver): Claude Code's three-tab questions (a single
  choice, a multi-choice with a typed answer, a two-line typed answer, then
  Submit), a Claude Code permission prompt and a Codex approval (both
  answered "No").
- From real screens but not driven live: the plan approval (driven like the
  permission prompt), one-question dialogs.
- Not tried live yet: the brain's decisions (auto-answer, escalate) and
  plain-text questions from a real finished turn; they are covered by tests
  with real finished messages (`finished-messages.json`) and fixtures.
- Dialogs on other machines don't get question sets yet (their screens
  aren't read); their plain-text questions do (read from the screen).
- "Chat about this", Claude's "Tab to amend" and a plan's "Tell Claude what
  to change" with text aren't offered: those are for the terminal.
- New Claude Code or Codex versions may draw their dialogs differently; the
  reader then finds no set (the old way still works), and the fixtures show
  what to update.
