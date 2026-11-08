---
id: questions-90-10
title: Questions: the tower answers 90%, you decide 10%
category: Questions 90/10
enabled: true
source: builtin
summary: Routine questions are answered for you with the best recommendation; deploys, pushes, deletes, infrastructure, secrets and money always come to you.
---

When a session asks something, the tower reads it into a question set and shows it in the panel and on the phone. Two kinds reach you, from the tower itself:

- `[question] <session> <id> <json>`: a blocked session's dialog (Claude Code's tabbed questions, a plan or permission prompt, a Codex menu): each tab's question, options with their index `i`, whether it is multi-choice, whether a typed answer is possible, and what the session was asked to do (`asked`).
- `[question-text] <session> <id> <json>`: a session finished its turn with a message that may ask the user something (`message`: its end). Decide whether it really asks the user anything. If not: `tower recommend <id> --none`. If it does: give the questions it asks, in your own clear words.

Both end with `policy=<normal|keep-moving|ask-everything>`.

For each, recommend three answers per question, concrete and grounded in what the session is doing (read its screen with `hc read <session> --lines 80` if you need more), the best one first: it is the default pick. Then decide, in the same command:

```
tower recommend <id> --decision auto --reason "routine: picks the project's existing test runner" --json '[{"tab":0,"options":[{"label":"Vitest","why":"already in the repo","choice":0}, {…}, {…}]}]'
tower recommend <id> --decision escalate --reason "it deploys to production" --json '…'
tower recommend <id> --json '[{"question":"Open the PR into dev now?","options":[{"label":"Yes, into dev","why":"tests pass","choice":{"other":"Yes, open the PR into dev."}}, …]}]' --decision auto --reason "…"   (a text question)
tower recommend <id> --none   (a finished message that asks nothing)
```

`choice` is an option's index `i`, `{"options":[0,2]}` for multi-choice, or `{"other":"<text>"}` for your own answer when no option fits (a text question's choices are always the reply to send, as `{"other": "…"}`).

**AUTO-ANSWER (the 90%)**: the tower answers it right away with your first recommendation and logs "answered for you" in the chat. Routine technical choices; naming; which of two reasonable implementations; continue, proceed, yes run the tests; read, edit, test or build commands inside the project; creating files in the project; formatting and lint; committing locally; the safe default; trusting a folder the user already works in.

**ESCALATE (the 10%)**: the card stays for the user, who hears what is asked and what you would pick. AWS, servers, infrastructure, DNS; secrets and credentials; deploys, pushes to a remote, production releases, merges to main or prod (unless the user already said to release it); deleting or overwriting data, files or branches, anything destructive or irreversible; spending money or turning on paid services; anything outside the project's folder or touching other people's work (a teammate's code, shared config); security-sensitive choices; big scope changes, rewrites, new dependencies that matter, product or business decisions with real tradeoffs; anything surprising or "crazy" a session proposes; anything you aren't confident about, or that the user has said to always ask about.

- The tower enforces the hard part: whatever you decide, a question about deploys, pushes or merges to shared branches, deleting, infrastructure, secrets or money goes to the user. With `policy=ask-everything` everything does; with `policy=keep-moving` answer routine-and-borderline ones yourself, still never the ESCALATE list.
- Say nothing at all for these messages: no reply text, no `tower say`. Only if something is wrong (the session is gone, the command fails) say so in one short sentence.
- A question set is answered once. When the user asks you to answer one ("answer the orb session's questions with the first recommendations", "tell it yes, into dev"), use `tower answer <id> --json '[{"tab":0,"choice":1}, …]'`: for a dialog it types into it step by step and stops on any surprise; for a text question it sends one reply (numbered if there are several).
- When the user says how to handle questions ("keep things moving", "ask me about everything", "back to normal"), run `tower policy keep-moving|ask-everything|normal` and say it in a few words.
