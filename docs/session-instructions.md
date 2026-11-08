# Instructions for sessions the tower runs

Paste this into a session's task (or its project's CLAUDE.md / AGENTS.md) so
its visual results show up where the user looks: the tower's panel on the
Mac, and the Tower app on the phone.

---

## Showing visual results to the user

If your task changes anything visual (a web page, an app screen, a chart, a
generated image, a UI component), finish by showing the result:

1. Take a screenshot of the result (a browser or simulator screenshot, a
   Playwright `page.screenshot()`, `adb exec-out screencap -p > shot.png`,
   `screencapture -x shot.png` on the Mac).
2. Run:

   ```sh
   tower screenshot <path> [<path> ...] --caption "<what it shows, in a sentence>"
   ```

   Several images at once are fine (before and after, several screens).
   PNG, JPEG, WebP or GIF, 15 MB each at most. The tower keeps its own copy,
   so you may delete yours.

It tells the tower which session sent it by itself (inside Herdr); add
`--session <terminal>` if you run it from elsewhere. It prints one line when
the screenshot is shown. If `tower` isn't found, use
`~/.local/bin/tower` (that's where it is installed; Herdr terminals have
`~/.local/bin` on their PATH).

---

Notes for the tower:

- Screenshots are history entries of kind `screenshot`: `session`,
  `account`, `caption`, `path` (the tower's copy, under
  `~/.local/share/herder-tower/attachments/screenshots/`), `at`, `id`, and
  `job` when the session had an open job. The daemon's `state.json` lists
  the latest 30 (`screenshots`); the phone API serves
  `GET /v1/screenshots` and `GET /v1/screenshots/<id>`.
- Copies are removed after 30 days. Nothing else in the attachments folder
  (files the user attached) is ever removed.
- `tower screenshots --last 10` lists them (JSON when piped).
