# Behaviors

Behaviors are how the tower works, written as rules the user can read and
change: how it talks, how it hands out and watches work, spaces, accounts,
releases and safety, which questions it answers itself, screenshots,
browsers. The Mac panel and the Android app have a Behaviors screen;
the tower also changes them when told to ("from now on …", "change the
screenshot behavior to …", "turn off …", "undo that").

Code: `src/behaviors.rs`. The tower's own behaviors ship in
`assets/behaviors/`; the fixed core is `assets/tower-core.md`.

## The store

`~/.config/herder-tower/behaviors/`, one markdown file per behavior:

```markdown
---
id: release-dev-then-prod
title: Release dev then prod
category: Releases and safety
enabled: true
source: learned
summary: Releases always go to dev first, then dev to prod
created_ms: 1794180000000
updated_ms: 1794180000000
---

Always release via dev first (PR merged into dev), then promote dev to prod. …
```

- `category`: Talking, Sessions, Spaces and focus, Accounts, Releases and
  safety, Questions 90/10, Screenshots, Browsers, Misc (in that order).
- `source`: `builtin` (shipped with the tower), `user` (added by the user in
  a screen or terminal), `learned` (from the brain's memory, or added by the
  brain when the user stated a standing preference).
- `summary`: one plain sentence for the user; the text below is the exact
  rule the brain reads.
- A builtin also has `builtin_hash` (its text as shipped): an unedited
  builtin follows new versions of the tower; an edited one is left alone; a
  removed one isn't brought back.

Every change is a line in `history.jsonl`: `{at, op, id, title, before,
after, by, undid}` (the whole file before and after), so `undo` restores
exactly: it undoes the latest change not undone yet, and again goes further
back.

## The brain's instructions

`brain/CLAUDE.md` (and `AGENTS.md` for a Codex brain) are generated:

1. a header: generated, edit via behaviors;
2. the **core**, fixed and read-only: who the tower is, how messages reach
   it (`[voice]`, `[watch]`, `[question]`, …), its tools and commands, and
   how behaviors work (below);
3. `# Behaviors`, then every enabled behavior under its category.

A change rewrites the files at once. The brain reads `CLAUDE.md` when it
starts; for a change made in a screen, the tower sends it
`[behaviors] <op> "<title>": read … again, and write nothing` (a quiet turn:
nothing is spoken). Changes the brain makes itself need no message.

## Seeding

`tower install` seeds the store: the builtins from `assets/behaviors/` (the
old single prompt split by section: You are heard, not read; Look before you
act, then hand off; Leave sessions running; Spaces and focus; Claude
accounts; Blocked sessions and approvals; Questions 90/10 with both lists;
Screenshots), and the brain's memory notes that are standing preferences
(`learned`, their whole text, the note's description as the summary):
autonomy for space sessions, be brief, say what a blocked session needs,
name sessions at startup, dev ports 3000-3050, open sessions in the named
space, release dev then prod, validate everything, the browser per account,
"dashboard" means eyzflow. The originals move to `behaviors/.migrated/`
and their lines leave the memory index; notes that are facts (the email
setup is done, who the accounts are) stay memory.

## Changing them

- **By voice or text**, in the chat or the Behaviors screen's own input
  (which sends to the tower like the chat does): the brain turns what the
  user said into `tower behavior add|edit|enable|disable|remove|undo`, a
  clean rewrite of the rule, and says one line ("Added to your behaviors.").
  When the user states a standing preference ("always …", "from now on …",
  "never …"), the brain adds a behavior (rather than only a memory note).
- **In a screen**: the phone API (below) or the panel through the daemon.
- **In a terminal**: `tower behavior list | show <id> | add --title "…"
  --category "…" --summary "…" "<rule>" | edit <id> [--text "…"] [--title …]
  [--summary …] [--category …] | enable <id> | disable <id> | remove <id> |
  undo | history | prompt` (`prompt` prints the generated instructions).

Every change shows in the chat as a small card (`kind: behavior`, its text,
and `diff`: the rule line by line, `+ ` added, `- ` removed), goes to the
phones as a `behaviors` event, and updates the state's `behaviors` summary.

## API (phone)

| Call | Body | Returns |
|---|---|---|
| `GET /v1/behaviors` | | `{"categories":[…], "behaviors":[{id,title,category,enabled,source,summary,created_ms,updated_ms,text}], "core":{"title","text","read_only":true}, "summary":{count,enabled,by_category,updated_ms}}` |
| `GET /v1/behaviors/{id}` | | one behavior |
| `POST /v1/behaviors` | `{title, category, summary, text}` | `201 {behavior, change}` |
| `PUT /v1/behaviors/{id}` | any of `{title, category, summary, text, enabled}` | `{behavior, change}` (`enabled` alone is an enable/disable) |
| `POST /v1/behaviors/{id}/preview` | `{text}` | `{id, diff}`: what an edit would change, not saved |
| `DELETE /v1/behaviors/{id}` | | `{change}` |
| `POST /v1/behaviors/undo` | | `{behavior, change}`; `400` with nothing to undo |
| `GET /v1/behaviors/history?limit=50` | | `{"changes":[{at, op, id, title, by, undid, diff}]}`, newest first |

A `change` is `{at, op: add|edit|enable|disable|remove|undo, id, title, by:
user|brain, undid, diff}`. SSE: `behaviors` with a change. The core is never
changed through the API.
