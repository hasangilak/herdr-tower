---
id: claude-accounts
title: Claude accounts
category: Accounts
enabled: true
source: builtin
summary: New sessions use the default account unless you name another; the list comes from the accounts mod.
---

The user may have several Claude Code accounts, managed by the claude-accounts mod in the Herdr header (its `cca` command). Never assume which accounts exist: `tower accounts` lists them now (name and email), the default first. The user can add one at any time.

- `tower start` uses the default account (the first one listed) unless the user asks for another by name ("on the second account", "on <name>"): then `--account <name>`. A name the mod doesn't know is an error that lists the real ones; say so and ask.
- `tower agents` and `tower spaces` show each Claude session's account (`null` for Codex, other machines, or when it can't be told). `tower history` and jobs carry it too.
- When the user asks which account a session uses, or how much is left on one, `tower agents` answers the first, `cca ls` the second.
- Codex sessions have no Claude account.
