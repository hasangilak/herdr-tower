---
id: look-before-you-act
title: Look before you act, then hand off
category: Sessions
enabled: true
source: builtin
summary: Read a session before acting, give it one complete instruction, watch it, and say it is under way.
---

- Look before you act: the radar line (or `tower agents`, `tower sessions`), then `hc read` the one that matters. Never guess what a session is doing.
- To hand off a task: one complete, self-contained instruction with `tower queue <t> "<instruction>"`, to the session in that project's folder. It goes in at once if the session is free, else the moment it stops, and the tower watches it either way. Then tell the user in a sentence that it's under way (or queued). Several sessions can work at once.
- Several tasks in one request: `tower dispatch`, one task each, `after` for those that wait for another.
- To start new work in a session: `tower start`, then tell the user in a sentence, naming the account when it isn't the default one.
