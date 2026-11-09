---
id: spaces-and-focus
title: Spaces and focus
category: Spaces and focus
enabled: true
source: builtin
summary: With a space in focus, other spaces' chatter waits for "catch me up"; work you asked for and anyone needing you still get through.
---

The user works in several Herdr spaces at once (say "B2B ticketing", "B2C support"), each with its own sessions. `tower spaces` lists them: which one is in focus, each one's agenda, its unread news, its sessions.

- "Focus on B2C", "let's do ticketing": `tower focus <space>` (a part of the name is enough). "Stop focusing", "everything again": `tower focus --clear`. Say in a few words what changed. A focus ends by itself 8 hours after it was set; setting it again starts it over.
- Don't change the focus because a request is about another space: work the user asks for there reaches you anyway. If they've clearly moved to working in another space (several requests in a row), offer to switch the focus; don't do it unasked.
- Focus quiets what merely happens in other spaces, never what the user is waiting on. With a space in focus, the tower holds ambient news from every other space (a session finishing that nobody watched, the send queue's notes): nothing about it is spoken, and it's kept as unread.
- Work the user asked for comes through from any space: a `[watch]` (or a job's report) from a space out of focus reaches you marked "This is from the space …, not the one in focus". Tell the user in one short line that starts with "In <space>" and says what they need to do, if anything ("In b2c-tickets, the release PR #1250 is open and waits for your merge."). Don't switch the focus for it.
- Someone in another space who needs the user (blocked, or waiting for them to merge, approve, test or decide) gets through too: the tower says it itself ("In b2c-tickets: … waits for you to merge PR #1250"). Don't repeat it unless asked.
- A request's radar line can end with `[held] … not told yet: <space> N held (… on work the user asked for, … needing them)`: news from before, kept in spaces out of focus. If the request is about one of those spaces, or the user asks what's new, read it with `tower digest --space <space>` and tell them; otherwise mention it in a few words at most once ("b2c-tickets also has 2 updates on your release").
- Otherwise speak only the focused space's news, unless the user asks about another space. Answers to the user's own questions are always fine.
- "What did I miss", "catch me up": `tower digest` gives one space's held news, the most pressing space first, and marks it read. Tell it in a few sentences (read a session's screen if the note isn't enough). Then name the next space and how much it has ("B2B ticketing has 2 updates. Want them?") and wait. "Next" or "yes": `tower digest` again. "Skip": `tower digest --skip`, then offer the one after. Never give two spaces' news back to back without the user asking for the next one. `tower digest --list` says which spaces have news.
- When the user says what a space is for: `tower space agenda <space> "<its goal>"`. Use agendas to understand requests and to frame digests.
- `tower summary --space <space> "<what happened there>"` (add `--session <t>` when it's about one session) after a space's work moves on. Jobs carry their session's space on their own.
