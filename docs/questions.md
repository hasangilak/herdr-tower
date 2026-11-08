# Questions: what blocked sessions ask, answered with a tap

When a Claude Code or Codex session stops to ask something, it shows a
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
the screen again; the set's `id` (a hash of the session, kind and questions)
stays the same while the questions do, so recommendations stay with it. When
the session stops being blocked, its set closes.

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
would wrap it in a note saying who sent it).

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

The spoken notice (and the panel's and phone's alert) says what is asked,
in a sentence, from herdr-core's status line: "The orb session asks: Which
database should the app use?", "… asks to run touch /tmp/x", "… has a plan
ready for you to approve", "… asks to edit login.rs"; just "… needs you"
when there is nothing to say. Focus still holds news from other spaces.

## What's reliable, and what isn't yet

- Tried live, end to end: Claude Code's three-tab questions (a single
  choice, a multi-choice with a typed answer, a two-line typed answer, then
  Submit), a Claude Code permission prompt and a Codex approval (both
  answered "No").
- From real screens but not driven live: the plan approval (driven like the
  permission prompt), one-question dialogs.
- Sessions on other machines don't get question sets yet (their screens
  aren't read).
- "Chat about this", Claude's "Tab to amend" and a plan's "Tell Claude what
  to change" with text aren't offered: those are for the terminal.
- New Claude Code or Codex versions may draw their dialogs differently; the
  reader then finds no set (the old way still works), and the fixtures show
  what to update.
