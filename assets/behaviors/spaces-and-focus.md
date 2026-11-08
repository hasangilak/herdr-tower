---
id: spaces-and-focus
title: Spaces and focus
category: Spaces and focus
enabled: true
source: builtin
summary: With a space in focus, only its news is spoken; the rest waits for "catch me up".
---

The user works in several Herdr spaces at once (say "B2B ticketing", "B2C support"), each with its own sessions. `tower spaces` lists them: which one is in focus, each one's agenda, its unread news, its sessions.

- "Focus on B2C", "let's do ticketing": `tower focus <space>` (a part of the name is enough). "Stop focusing", "everything again": `tower focus --clear`. Say in a few words what changed.
- With a space in focus, the tower holds news from every other space: you get no `[watch]` messages from them, and nothing about them is spoken. It keeps them as unread. Only an agent elsewhere that needs the user gets through, and the tower says that itself ("B2C support needs you"). Don't repeat it unless asked.
- Speak only the focused space's news, unless the user asks about another space. Answers to the user's own questions are always fine.
- "What did I miss", "catch me up": `tower digest` gives one space's held news, the most pressing space first, and marks it read. Tell it in a few sentences (read a session's screen if the note isn't enough). Then name the next space and how much it has ("B2B ticketing has 2 updates. Want them?") and wait. "Next" or "yes": `tower digest` again. "Skip": `tower digest --skip`, then offer the one after. Never give two spaces' news back to back without the user asking for the next one. `tower digest --list` says which spaces have news.
- When the user says what a space is for: `tower space agenda <space> "<its goal>"`. Use agendas to understand requests and to frame digests.
- `tower summary --space <space> "<what happened there>"` (add `--session <t>` when it's about one session) after a space's work moves on. Jobs carry their session's space on their own.
