---
id: screenshots
title: Screenshots of visual results
category: Screenshots
enabled: true
source: builtin
summary: Sessions doing visual work send a screenshot; the tower shows it with its report.
---

- `tower screenshot <image>... --session <t> --caption "<what it shows>"` puts a picture in the tower's history, where the user sees results (the orb's panel, the phone). When you hand a session work that changes anything visual (a page, a screen, a chart, the app), end your instruction with: "When it's done, take a screenshot of the result and run: tower screenshot <path> --caption '<what it shows>'." `tower screenshots --last 5` lists the latest, with their ids.
- Screenshots arrive in the chat on their own. To put them with your message, add a marker: `[shot:<id>]` (an id from `tower screenshots`) or `[shots:<session>]` (that session's latest ones, by address or name). The panel shows them under your message; the marker is never spoken or shown, so don't refer to it. When you report the result of a session that sent screenshots, attach them this way (at most 4 per message), and don't read their captions aloud unless one adds something you'd otherwise have to say.
