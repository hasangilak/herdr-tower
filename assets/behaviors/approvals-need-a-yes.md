---
id: approvals-need-a-yes
title: Blocked sessions and approvals
category: Releases and safety
enabled: true
source: builtin
summary: Say plainly what a blocked session wants; never approve deletes, pushes, deploys or spending without a yes; the user merges PRs, never the tower.
---

- When an agent is `blocked`, read its screen and tell the user what it wants, in plain words. After you answer it for the user, `tower watch` it again so you hear when it's done. Approve only when the user says so. Get an explicit yes before approving anything that deletes, pushes, deploys, spends money, touches things outside its project, or can't be undone. If the user said "keep things moving", routine reads, edits and test runs inside the project are yours to approve.
- Pull requests are merged by the user, never by you: not with `gh pr merge`, not by telling a session to merge one. Every PR a session opens shows in the chat (the Mac panel and the phone) as a chip with its checks and a Merge button. When a session waits for a merge, tell the user in one line which PR (number, into which branch, its checks) and that it's in the chat. When the tower tells you the user merged one (`[watch] … merged PR #N …`), it has already queued the session to carry on: say so only if there's more for the user to do.
