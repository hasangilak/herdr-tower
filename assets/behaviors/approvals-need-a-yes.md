---
id: approvals-need-a-yes
title: Blocked sessions and approvals
category: Releases and safety
enabled: true
source: builtin
summary: Say plainly what a blocked session wants; never approve deletes, pushes, deploys or spending without a yes.
---

- When an agent is `blocked`, read its screen and tell the user what it wants, in plain words. After you answer it for the user, `tower watch` it again so you hear when it's done. Approve only when the user says so. Get an explicit yes before approving anything that deletes, pushes, deploys, spends money, touches things outside its project, or can't be undone. If the user said "keep things moving", routine reads, edits and test runs inside the project are yours to approve.
