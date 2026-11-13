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
| Claude Code permission | `Do you want to …?` with options right under it: `Do you want to proceed?` (a command), `Do you want to make this edit to x.md?`, `Do you want to create x.md?` (a file) | `claude-permission`, with `target` (Herdr's line in full: `Update: /path`) |
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

A dialog it doesn't know by its question is a question set too, when the
screen ends on a menu: numbered options, one under the cursor, and `Esc to
cancel` under them; its question is the line over them that asks (`…?`).
It is a `claude-permission` (answered by its option's number), for the brain
and, when put to the user, with its options on the Mac and the phone. Only a
screen with no such menu gives no question set (the `blocked` card).

Every change of herdr-core's status line (a new tab, a new question) reads
the screen again, and so does the user answering the dialog in the terminal
tab by tab (which changes nothing in herdr-core's status): while a dialog of
several questions is open, its screen is looked at again about every 1.5
seconds, and read again whole when it shows another tab or other tabs
answered (`Asks::follow`, `questions::dialog_moved`). `current_tab` and each
tab's `answered` (and `answer`: the review tab lists the answers given, a
multi-choice tab shows its ticks) follow the dialog, and so do the cards on
the Mac and the phone, and the decision's title; the set's `id` (a hash of the session, kind, title, detail
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
     "multi": false, "allows_other": true,
     "answered": false,             // ticked in the dialog's tab bar (answered there)
     "answer": "SQLite"},           // what it was answered with, when the screen shows it (left out otherwise)
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

When the user replies in the session's own terminal instead (its
`UserPromptSubmit` hook brings the words, and the tower didn't type them),
the set closes as answered with those words, and a reply the tower held for
it is dropped, never typed (the record says "Not sent: you answered it in
the terminal"). A question at the end of a turn the user typed there goes
to them as it is: escalated at once (`reason`: "asked at the end of a turn
you typed in its terminal"), with no `[question-text]` note to the brain,
and not said. docs/quiet-tower.md, "Answered in the session's own
terminal", has the whole of it.

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
  (At the quiet levels of `tower tell` the tower says it itself either way,
  once per decision: see "Decisions" below.)

Idle is not blocked: a turn that ends saying only that the session is
ready ("I'm waiting for your instructions", "I have no task yet":
`questions::ready_only`), from a session nothing was asked of (no
instruction queued for it, no agenda task it is on, or one that itself told
it to wait for instructions), makes no card, no alert and no decision.

## Decisions

A set that is the user's (`decision: escalate`, a wait card too) is a
**decision** (`src/decisions.rs`, docs/quiet-tower.md step 3): one per
space and what is asked. The same thing asked by another session of the
space joins that decision (`waiting` counts them) instead of making a
second one; each session keeps its own set here. The decision's options are
the recommendations (the first is the pick), and `tower answer <decision
id> --choice <n>` answers every set in it. It closes with its sets.

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

- **A shell command pushes or merges only by its git and gh commands**
  (`questions::shell_command`, `without_data`, `pushes_or_merges`): for a
  Claude Code "Bash command" prompt or a Codex menu, the push/merge part of
  the rule reads the command itself: a real `git push`, `git merge` (past
  git's options: `git -C <dir> push`, `git -c k=v merge`), `git subtree|lfs
  push`, `gh|glab pr|mr merge`, `gh repo sync`, a `gh api` call on a
  `/merge` path, also as words a script runs (`["git", "push"]` in a Python
  heredoc). Not what the command only writes (the body of a heredoc fed to
  `cat` or `tee`, a message read by `git commit -F -`, the words of `echo`
  and `printf`), not prose like "merged" or "pushed", and not what the
  session was asked (its task saying "Don't merge or push." made every
  prompt of that session a push). `git merge-base`, `git log --merges` and
  `git stash push` are none. A heredoc whose end isn't shown is read whole.
  Questions in words ("Want me to merge this into main?") are read as before.
- **A file in Claude's own folder gets the plain yes** (`questions::
  memory_yes`, `file_write_targets`, `claude_folder_file`): a file prompt
  (Edit, Update, Write: "Do you want to make this edit to x.md?") whose file,
  in full from Herdr's line, and every file it says it modifies through a
  symlink ("This will modify <path> … via a symlink") are in a Claude folder
  (`~/.claude/**`, `~/.claude-accounts/<name>/**`, any
  `…/claude-profiles/<id>/home/**`), and are the file its question names,
  is answered by the tower at once with its plain one-time yes (never "Yes,
  allow all edits"): no brain, no card. Not the files that decide what a
  session may do or that run code: `settings*.json`, `.claude.json`,
  `.credentials.json`, `hooks/`, `plugins/` (those go to the brain). It
  came from a memory edit through orca's profile that waited 20 minutes as
  a bare card. With `ask-everything` it is the user's like every other.
- **Claude's own memory and notes get the plain yes** (`questions::
  memory_yes`, `Asks::memory_rule`): a permission prompt whose every
  sensitive-file target ("Claude requested permissions to edit <path> which
  is a sensitive file", and where it "resolves through a symlink to") is a
  file of Claude's own memory or notes (`projects/<project>/memory/…` or a
  project's `*.md` under `~/.claude`, `~/.claude-accounts/<name>`, or a
  profile's `…/claude-profiles/<id>/home` such as orca's under Application
  Support), and whose command does nothing this rule names once what it
  writes is left out (the session's task no part of it), is answered by the
  tower at once with its plain one-time yes, never an option that stops
  asking; the brain isn't asked and no card is made. With `ask-everything`
  it is the user's like every other prompt. Any other target, a second
  target that isn't memory, a reason it doesn't know, or a command that also
  pushes, deletes or the like: as before.
- `cargo test replay_a_captured_prompt -- --ignored --nocapture` with
  `HERDER_TOWER_REPLAY=<a screen>` (and `HERDER_TOWER_REPLAY_CONTEXT=<what
  the session was asked>`) reads a captured screen through the question
  reader and this rule, and `…_through_the_tower` through the daemon's
  decision (nothing is typed).

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

What was answered in the dialog already stands: answers for tabs it shows
answered are left out, and the keys start from the tab it is on. When every
answer given was for such a tab, nothing is typed and the answer says so
("Nothing was typed: … answered in its terminal already"); with every tab
answered there, `"answers": []` with `submit` only submits. The tower never
answers a dialog itself (`auto`) once the user is in it (a tab answered
there, or the dialog on another tab than it opened on): it becomes the
user's card, without a word.

On any surprise it stops and says where (`status: failed`, `error`), and
the user can finish in the terminal. When they do (or answer it there in
the first place), the radar sees the session go on: the set closes, its
decision closes as answered, the record says "Answered in <space ›
session>: <what the screen shows was chosen>", and no key the tower was
about to press goes in (the driver's keys stop once the set is closed).
Text is typed with `hc send` without
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
sentence with the tower's pick, once per decision: at `tower tell
decisions|results` the same thing asked by another session of the space
says nothing (at `everything` each set is said, as before). Other blocked sessions (and dialogs the tower
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
