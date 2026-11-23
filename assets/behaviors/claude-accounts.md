---
id: claude-accounts
title: Claude accounts
category: Accounts
enabled: true
source: builtin
summary: New sessions use the default account unless you name another; the list comes from the accounts mod.
---

The user may have several Claude Code accounts, managed by the claude-accounts mod in the Herdr header (its `cca` command). Never assume which accounts exist: `tower accounts` lists them now (name and email), the default first. The user can add one at any time.

- `tower start` uses the default account (the first one listed) unless the user asks for another by name ("on the second account", "on <name>"): then `--account <name>`. "The second account" is the second one listed, and part of a name will do. A name the mod doesn't know is an error that lists the real ones: start on the closest of those (or the default) all the same, and say which you used.
- `tower agents` and `tower spaces` show each Claude session's account (`null` for Codex, other machines, or when it can't be told). `tower history` and jobs carry it too.
- When the user asks which account a session uses, or how much is left on one, `tower agents` answers the first, `cca ls` the second.
- A session or a tower (you, or a space's own) that hits its account's usage limit is moved by the tower itself: to the next account with usage, in its own terminal, with its conversation, its queue and its agenda tasks, and what it was doing sent again. The user is told in one line. Don't restart it, send its task again, or open a new session for it. While an account is out, `tower start` without `--account` starts on the next one with usage. `tower failover` says what is out, what moved and what waits. When every account is out, the user already has one decision saying when the first is back: don't raise another.
- Each account has a browser its links open in: its sessions' Artifacts and sign-in pages, and the links the tower opens of it (an Artifact opens in the account that published it, whose browser is signed in to it). `tower accounts browser` lists them (Chrome for the default account, Brave for orca unless the user chose others); when the user asks for another ("open orca's links in Safari"), `tower accounts browser <account> <brave|chrome|safari|firefox|arc|edge|system>`. To open a link for the user on the Mac, `tower open <link> [--session <address>]`, never plain `open`.
- Each account has its own MCP servers (claude.ai connectors, plugins' servers, local ones), and they differ: `tower accounts` lists them under each account (connected, or needing sign-in), and "MCP servers by Claude account" at the end of your instructions says which has which. When a task needs one (the user names Figma, Sentry, YouTrack, a server), say so: `tower start --needs-mcp <name>`, `tower launch --needs-mcp <name>`, `"needs_mcp": ["<name>"]` on a dispatch task. The tower picks an account that has it connected and says which and why, or that none has it: tell the user that in a sentence. Don't pick `--account` for an MCP yourself; never tell the user an account has a server only because it is listed as needing sign-in.
- Codex sessions have no Claude account.
