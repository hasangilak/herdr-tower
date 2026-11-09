# The tower's phone API (v1)

The Android app talks to the tower's voice daemon over the local network. It
can show everything the orb's panel shows, act on the tower the way the panel
does, and be the tower's voice: you speak into the phone, the Mac's Whisper
transcribes it, and the tower's replies come back as text and as Kokoro audio
played on the phone.

Source: `src/daemon/lan/` (server, auth, TLS, hub, audio), `src/phone.rs`
(`tower phone`). Tests: `cargo test lan` (with an HTTPS client that pins the
certificate the way the app must).

## Turning it on

The API is **off by default**.

```sh
tower phone enable      # [lan] enabled = true in config.toml, and starts it if the voice runs
tower phone pair        # a one-time code and a QR for the app
tower phone devices     # paired phones, and whether one is the voice now
tower phone revoke <id> # or `all`: those tokens stop working at once
tower phone disable     # stops the API and ends open event streams (phones stay paired)
```

`config.toml`:

```toml
[lan]
enabled = false
port = 7843
bind = "auto"         # this Mac's address on its default-route network, or an IP of this Mac
allow_public = false  # refuse anything but private/link-local/100.64.0.0/10 (Tailscale) addresses
```

The daemon's `state.json` (and `GET /v1/state`) has `lan`
(`https://192.168.1.20:7843`, or `""` when off) and `phone` (the name of the
phone that is the voice, or `null`).

## Security model

What it protects: the tower can type into every agent session on every
machine (and approve what they ask). Whoever can call this API can do that.
So:

1. **Off unless enabled**, and it listens **only on this Mac's private LAN
   address** (not `0.0.0.0`; a public address is refused unless
   `allow_public = true`).
2. **TLS always** (TLS 1.3 or 1.2, rustls). The Mac makes a self-signed
   ECDSA P-256 certificate once (`~/.config/herder-tower/lan/`, key mode
   0600). There is no CA: the phone **pins the certificate's SHA-256
   fingerprint** (of the DER), which it gets out of band, in the pairing QR
   shown on the Mac. A changed IP doesn't matter; a different certificate
   (someone in between, another machine) fails the handshake. Hostname
   checks are not used (the certificate's name is `herder-tower`).
3. **Pairing by a one-time code shown on the Mac**: 8 digits, valid 5
   minutes, one use. Wrong tries are counted by address: after 5 from one
   address the code is void for that address (others can still pair), and
   after 30 in all it is void for everyone. Each wrong try also waits a
   second. Opening a new code replaces the old one.
4. **A long-lived random token per device** (`htk_` + 256 random bits,
   base64url), returned once at pairing. The Mac stores only its SHA-256
   (`~/.config/herder-tower/devices.json`, mode 0600) and compares in
   constant time. The phone keeps it in Android Keystore-backed storage
   (EncryptedSharedPreferences or a Keystore-wrapped DataStore).
5. **Every call needs `Authorization: Bearer <token>`**, the event stream and
   audio downloads included. The only unauthenticated call is
   `POST /v1/pair`, and it does nothing without the open code.
6. **Revocation** (`tower phone revoke <id|all>`, or the app's
   `DELETE /v1/device`): the token stops working on the next call, and open
   event streams of that device close within 5 seconds.
7. **Abuse limits**: 20 failed logins (bad token or code) from one address in
   a minute and it's refused for the rest of the minute; 32 connections at
   most, 8 from one address (more are closed unanswered); the TLS handshake
   and the request head must arrive within 10 s; request heads up to
   16 KiB. The token is checked from the head, **before any body is read**,
   and each call has its own body limit (`413 too_large` beyond it):
   `POST /v1/pair` 4 KiB; a plain WAV (`/v1/voice`, `/v1/transcribe`,
   `/v1/questions/{id}/voice`) 24 MiB; multipart (`/v1/ask`, the voice
   calls, `/v1/questions/{id}/answer`) 100 MiB; every other call 1 MiB.
   Recordings are 0.2 to 120 s (checked from the WAV header).
8. **Text from the phone** that is typed into a session (`/v1/ask` text,
   typed answers to questions, an approve/refuse note) loses its control
   characters (C0 and C1, escape sequences' introducers among them), except
   newlines and tabs. An address must start with a letter or digit.

What's left, honestly: anyone on the same network can see that a TLS server
runs on the port, and could try codes during the 5 minutes a code is open
(5 tries from an address, 30 in all: at worst 1 in 3 million), or use up
the 30 from several addresses so the code has to be made again. A phone that is stolen while paired holds a valid
token until you revoke it (`tower phone revoke`). The certificate's private
key is as safe as your Mac account. Off the local network (later: Tailscale,
which `allow_public = false` already accepts as 100.64/10) the same model
holds.

## Pairing flow

1. On the Mac: `tower phone pair`. It prints a QR, the address, the code and
   the certificate fingerprint. (The orb can show the same:
   `tower phone pair --json`.) The QR is a link:

   ```
   herdertower://pair?h=192.168.1.20&p=7843&c=<8 digits>&fp=<64 hex chars>
   ```

2. The app scans it (or the user types address and code; then the app shows
   the fingerprint of the certificate it got, grouped by four, and the user
   compares it with the one the Mac printed before the app goes on).
3. The app calls, pinning `fp`:

   ```http
   POST /v1/pair
   Content-Type: application/json

   {"code": "<the 8 digits>", "name": "Pixel 9"}
   ```

   ```json
   200 {"token": "htk_…", "device": {"id": "d_3fa9c2e1", "name": "Pixel 9"}, "fingerprint": "<hex>", "api": 1}
   403 {"error": {"code": "pairing_closed", "message": "…"}}
   403 {"error": {"code": "wrong_code", "message": "…", "tries_left": 4}}
   ```

4. The app stores host, port, fingerprint and token, and from then on sends
   the token with every call.

## Conventions

- Base: `https://<host>:<port>/v1`. JSON in and out (UTF-8), except audio.
- One request per connection (`Connection: close`); bodies need
  `Content-Length` (no chunked uploads). OkHttp does both by default.
  Send the `Authorization` header with the body's call: a call without a
  valid token is answered `401` from its head alone, its body unread.
- Times are Unix milliseconds (`*_ms`).
- Errors: `{"error": {"code": "<word>", "message": "<for a person>"}}` with
  status 400 (bad request), 401 `unauthorized` (missing, unknown or revoked
  token: re-pair), 403 (pairing), 404, 413 (a body over the call's limit,
  `too_large`; files), 415 (audio, files), 429
  `too_many_failures`, 503 (the brain, `hc`, or the transcriber isn't there).
- Addresses (`t3`, `vps/t3`) are URL-encoded in paths: `vps%2Ft3`. A `+`
  in a path is a plus; only in a query string is it a space.

## Reading: what the panel shows

| Call | Returns |
|---|---|
| `GET /v1/hello` | `{"api":1,"tower":"0.1.0","device":{"id","name"},"voice":"af_bella","phone_is_voice":bool}`. Use it to check a stored token. |
| `GET /v1/state` | The daemon's state (below). Also pushed on the event stream. |
| `GET /v1/agents?all=1` | `tower agents --json`: `{"agents":[…],"unreachable":[…]}`; each agent has `address, account, machine, name, spoken, agent, status, doing, title, folder, where`. `all=1` adds plain shells. Slow-ish (asks Herdr, and other machines). |
| `GET /v1/spaces` | `tower spaces --json`: `{"focus": name or null, "spaces":[{"space","focus","agenda","unread","unread_asked","unread_needs_you","running_jobs","sessions":[{"address","name","status","account"}],"last_activity_ms"}]}`. `unread`: held news not yet read; of it, `unread_asked`: reports on work the user asked for (a watch's or a job's), `unread_needs_you`: someone there blocked on or waiting for the user. Show a space with either above 0 as needing a look (the panel marks it amber). |
| `GET /v1/jobs?status=open\|finished\|all&session=t3&limit=50` | `{"jobs":[{"job","session","name","space","account","status","asked","summary","started_ms","updated_ms"}]}`, newest first. `status`: `working`, `blocked`, `done`, `failed`, `error`, `exited`. |
| `GET /v1/history?limit=100&session=t3&kind=summary,answer&before_ms=…` | `{"entries":[…]}`, oldest first, the latest `limit`. Page back with `before_ms` = the first entry's `at`. Summaries are `kind=summary`. |
| `GET /v1/chat?before=h120&limit=50` | `{"messages":[…], "more": bool}`: **the main chat thread**, the same one the Mac panel shows, newest last; `before` pages back (the first message's `id`), `more` says older ones remain. See "The chat" below. |
| `GET /v1/questions` | `{"questions":[…]}`: what blocked sessions ask, as question sets (see [questions.md](questions.md)); also in the state (`questions`; each agent's `question` id). A set with `kind: "text"`, `decision: "escalate"` and `reason: "it waits for you to do this"` is a session that stopped for the user to act (merge, approve, test, decide): its `title` is the request; no recommendations will come (show "waits for you" and a reply box, not a spinner). |
| `GET /v1/questions/{id}` | One set. `404 no_question`. |
| `GET /v1/prs` | `{"prs":[…]}`: the PRs the tower's sessions opened, open ones and those closed in the last day, waiting ones first (see "PRs" below). |
| `GET /v1/prs/{owner}/{repo}/{number}` | `{"pr":{…}}`. `404 no_pr`. |
| `GET /v1/questions-policy` | `{"policy": "normal"\|"keep-moving"\|"ask-everything"}`: how questions are handled (also `question_policy` in the state). |
| `GET /v1/behaviors` | The user's behaviors (how the tower works), the fixed core and a summary; add, edit, enable/disable, remove, preview, undo and history: see [behaviors.md](behaviors.md). |
| `GET /v1/accounts` | `{"default": name, "accounts":[{"name","email"}]}`, the default first (from the claude-accounts mod; names are not fixed: colour by name). |
| `GET /v1/screenshots?limit=30&session=t3` | `{"screenshots":[{"id","at","session","name","space","account","job","caption","path","url"}]}`, newest first (still on disk: kept 30 days). |
| `GET /v1/attachments/{folder}/{name}` | A picture someone sent with a message (the image itself, by its bytes: `image/png`, `image/jpeg`, `image/webp`, `image/gif`). `{folder}/{name}` is the end of its path in a `request` entry's `attachments` (`…/attachments/1794165000000-phone/1-login.png` → `/v1/attachments/1794165000000-phone/1-login.png`), URL-encoded (panel attachments keep their original names, spaces and all). Only files directly in a folder of `attachments/`; `400 bad_path`, `404 no_attachment`, `415 not_an_image` (videos and other files aren't served). |
| `GET /v1/screenshots/{id}` | The image itself (`image/png`, `image/jpeg`, `image/webp` or `image/gif`). `400 bad_id`, `404 no_screenshot`. |
| `GET /v1/sessions/{address}/screen?lines=60` | `{"address","screen"}`: what that terminal shows (to see what a blocked agent asks). Up to 400 lines. |

A history entry: `{"at", "kind", "session"?, "name"?, "space"?, "job"?,
"account"?, "status"?, "text", "attachments"?, "held"?}`. `kind`: `request`
(the user), `answer` (the tower), `watch`, `alert`, `job`, `summary`, `note`,
`screenshot` (with `id`, `caption`, `path`; fetch it from `/v1/screenshots/{id}`).
Keys are left out when empty.

The state (`GET /v1/state`, event `state`):

```json
{
  "version": 812, "updated_ms": 1794158000000, "pid": 28224, "running": true,
  "phase": "idle",
  "mode": "ptt", "mic": "closed", "armed": false, "transcribing": false,
  "speaking": false, "announcing": false,
  "brain": "done", "brain_app": "claude-code",
  "hearing": "", "heard": "what's everyone doing", "heard_ms": 0,
  "said": "Nothing is blocked.", "said_ms": 0,
  "alerts": [{"at_ms": 0, "terminal": "t22", "text": "Claude in eyzflow is done", "level": "done"}],
  "agents": [{"address": "t22", "name": "Claude in eyzflow", "status": "working", "app": "claude-code", "account": "default"}],
  "error": "", "talk_key": "ctrl", "voice": "af_bella", "stt_model": "…",
  "lan": "https://192.168.1.20:7843", "phone": "Pixel 9",
  "work": {"word": "Reading", "detail": "eyzflow", "since_ms": 1794158000000, "at_ms": 1794158004000, "done_ms": 0},
  "screenshots": [{"id": "1794160000000-login.png", "at": 1794160000000, "session": "t22", "name": "Claude in eyzflow", "space": "B2C support", "account": "default", "job": null, "caption": "The new login page", "path": "/Users/…/attachments/screenshots/1794160000000-login.png", "url": "/v1/screenshots/1794160000000-login.png"}]
}
```

`work` is what the brain is doing right now, as the panel's working line shows
it ("Delegating to orb-rust · 12s"): `{"word": "Delegating", "detail": "to
orb-rust", "since_ms": <turn began>, "at_ms": <this step began>, "done_ms":
<step finished, 0 while it runs>}`, or `null` between turns. A `state` event
is sent when it changes too.

`phase` (one word for the orb): `announcing`, `speaking`, `transcribing`,
`listening`, `thinking`, `blocked`, `armed`, `standby`, `idle`. Unknown keys
may be added later: ignore them.

## Acting: what the panel does

| Call | Body | Does |
|---|---|---|
| `POST /v1/ask` | `{"text": "what's everyone doing?"}`, or multipart with images (below) | Sends it to the tower like typing in the panel (it arrives as `[voice] …`). `202 {"sent": text, "attachments": [paths]}`. The canned "Okay." is played (on the phone when it's the voice). |
| `POST /v1/agents/{address}/answer` | `{"decision": "approve"\|"refuse", "note": "only the read"}` | Tells the tower, as the user would: "Approve what t22 is asking for. only the read". The tower reads the session's screen and answers it, as today. `202`. |
| `POST /v1/sessions/{address}/show` | | Brings that session on screen in the Mac's Herdr window (as `tower show <t>`: focuses its tab, or gives it one). `200 {"shown": address}`, `404 no_session`, `400 bad_address`, `503 herdr` (the window isn't open). |
| `POST /v1/focus` | `{"space": "B2C support"}` or `{"space": null}` | Focuses a space (its news is spoken, others held) or clears focus. `200 {"focus": name or null}`; `404 no_space`. |
| `GET /v1/device/audio` | | `200 {"route": "phone"\|"mac"}`: where this phone's replies are heard (below). |
| `PUT /v1/device/audio` | `{"route": "phone"\|"mac"}` | Sets it (kept in the Mac's devices.json, so it survives restarts). `400 bad_route`. Also in `hello` and the state as `audio_route`. |
| `GET /v1/audio?after=<id>` | | `{"clips": [{"id","kind","text","ms","url","at_ms"}]}`: clips meant for this phone (its own and the voice's) from the last 15 minutes, after clip `after`: what it missed while no stream was open. Empty while the tower is muted. |
| `POST /v1/hush` | | Stops speaking now, and drops what's queued (the phone gets `hush`). |
| `GET /v1/queue` | | `{"items": [{"id","session","name","text","at_ms","tries","parked"?,"parked_ms"?,"after"?}]}`: instructions the send queue holds for busy sessions (`tower queue`), oldest first. `parked` says why one won't go on its own (`gone`, `replaced`, `after-cancelled`, `start-failed`), since `parked_ms`; one parked for a week leaves the list (kept aside as expired; `tower queue retarget` still sends it). Each agent in the state also has `folder` and `queued` (how many wait for it). |
| `GET /v1/mute` | | `{"muted": bool}`: the tower-wide mute (`tower mute`). The state has it too (`muted`). |
| `POST /v1/mute` | `{"muted": true}` | Mutes the tower (or unmutes it with `false`): it stops talking now (the phones get `hush`), the clips waiting to be fetched are dropped, and it says nothing on the Mac or any phone until unmuted. Text, chat and questions go on. Kept across restarts. |
| `POST /v1/mode` | `{"mode": "ptt"\|"handsfree"\|"off"}` | The Mac microphone's mode. |
| `POST /v1/questions/{id}/answer` | `{"answers":[{"tab":0,"choice":1},{"tab":1,"choice":{"options":[0,2],"other":"…"}}],"submit":true}`; or multipart: that JSON as a part named `json`, files for tab N's typed answer as parts named `tab<N>` | Types the answers into the session's dialog, checking the screen after each step, then submits after checking the review. `200 {"ok":true,"id","submitted"}`; `409 {"error":{"code":"stopped"\|"changed"\|"answered"\|"cannot_answer","message":"where and why"}}` (`answered`: the dialog was answered in the session meanwhile, and the set is closed; `changed`: the dialog asks something else now, and the set under its new id is in the questions, with `replaces` naming the old one when it is the same dialog read again); `400 bad_tab` (an answer for a tab the set doesn't have; nothing is typed or saved); `404`. Every tab's files are checked before any is saved. |
| `DELETE /v1/questions/{id}` | | Dismisses it (a text question that isn't one, or one the user answers in the terminal). |
| `PUT /v1/questions-policy` | `{"policy": "normal"\|"keep-moving"\|"ask-everything"}` | Saved in the Mac's config. |
| `POST /v1/prs/{owner}/{repo}/{number}/confirm` | `{}` | Step one of a merge: GitHub is asked again, and the answer says what merging would do: `{"token","expires_ms","pr":{…},"method":"merge"\|"squash"\|"rebase","can":bool,"blocked":null\|"why","needs_override":null\|"what fails","warnings":["…"]}`. Show it to the user; the token is for this phone, this PR and its head commit as shown, for two minutes, once. `404 no_pr`, `503 github`. |
| `POST /v1/prs/{owner}/{repo}/{number}/merge` | `{"token":"…"}` or `{"token":"…","override":true}` | Step two: merges with the repo's default method, pinned to the commit confirmed, never forced. `override: true` only after the user said a second time that the failing checks are unrelated. `200 {"merged":true,"pr":{…}}`; `403 bad_token` (missing, used, another PR or device, expired: confirm again); `409 {"error":{"code":"blocked"\|"needs_override"\|"changed"\|"merge_failed","message":"…"}}` (`changed`: new commits since the confirmation; `merge_failed`: GitHub refused, e.g. a required review). The token is used up either way. |
| `POST /v1/questions/{id}/interpret` | `{"tab":0,"text":"the second one"}` | `{"tab","choice":{"options":[1],"other":null}}`: a spoken answer as a choice, to show before sending. |
| `POST /v1/questions/{id}/voice?tab=N` | a WAV take | `{"tab","heard","choice"}`: transcribed by the Mac's Whisper, then as `interpret`. |
| `DELETE /v1/device` | | Unpairs this phone (its token stops working). |

### Files with an ask

`POST /v1/ask` also takes `multipart/form-data` (OkHttp `MultipartBody`,
`MultipartBody.FORM`): a `text` field and up to **6 file parts** (any part
with a file name, or named `image…`/`file…`):

- **images**: PNG, JPEG, WebP or GIF (checked by their bytes; a file named
  as an image that isn't one is refused), **15 MB** each;
- **videos** (`.mov .mp4 .m4v .webm .mkv .avi`, or `Content-Type: video/*`)
  and **any other file** (PDF, text, …): **50 MB** each.

```
POST /v1/ask
Content-Type: multipart/form-data; boundary=…

--…  Content-Disposition: form-data; name="text"
what's wrong on this page? [Image #1]
--…  Content-Disposition: form-data; name="image"; filename="login.png"   Content-Type: image/png
<bytes>
--…  Content-Disposition: form-data; name="image"; filename="console.jpg"  Content-Type: image/jpeg
<bytes>
--…--
```

Every file is checked before any is saved (one refused file: nothing is
saved). The Mac saves them as the panel saves its attachments, in a new
folder per message
(`~/.local/share/herder-tower/attachments/<ms>-phone-<8 hex>/<n>-<name>.<ext>`;
attachments are never pruned) and the tower gets exactly what the panel
sends: `[voice] <text> [attached: <path 1> | <path 2>]`, where the text has
the panel's token for the Nth file: `[Image #N]`, `[Video #N]` or
`[File #N: <file name>]`. Put the tokens where the user put the files; any
file the text doesn't mark gets its token in front (`[Image #2] what's wrong
on this page? [Image #1]`; a `[File #N:` token counts by its number). Text
may be empty when there are files.

```json
202 {"sent": "[Image #2] what's wrong on this page? [Image #1]", "attachments": ["/Users/…/attachments/1794165000000-phone-3fa9c2e1/1-login.png", "…/2-console.jpg"]}
413 {"error": {"code": "too_many_files" | "image_too_large" | "file_too_large", …}}
415 {"error": {"code": "not_an_image", …}}
400 {"error": {"code": "bad_multipart" | "empty", …}}
```

A multipart body is 100 MiB at most; a JSON ask 1 MiB.

## The chat

There is one chat thread, the tower's, on every screen: the Mac panel and
every phone show the same messages. It lives on the Mac in the history
(`history.jsonl`, kept across restarts); what the phone sends goes into it
(marked with its source) and shows in the panel, and the other way round.

A message:

```json
{
  "id": "h812", "at": 1794158000000, "updated_at": 1794158000000,
  "role": "user",                // user | tower | screenshots | event
  "kind": "request",             // the history's: request, answer, screenshot, watch, alert, job, summary
  "text": "[Image #1] [File #2: notes.pdf] compare these",
  "source": "phone:d_9fce5c6c",  // mac (typed in the panel, tower ask) | voice (the Mac's microphone) | phone:<device> | tower
  "attachments": [
    {"n": 1, "kind": "image", "name": "1-login.png", "path": "/…/attachments/…/1-login.png", "url": "/v1/attachments/1794-phone/1-login.png"},
    {"n": 2, "kind": "file",  "name": "2-notes.pdf", "path": "/…/2-notes.pdf", "url": null}
  ],
  "screenshots": []
}
```

- **user**: what the user asked, typed or spoken, from any screen. The text
  keeps the panel's tokens; `attachments[n-1]` is the file for token `#n`
  (`kind` `image`, `video` or `file`; pictures have a `url`).
- **tower**: an answer. Screenshots it attached (`[shot:…]` in what it
  wrote; never in `text`) are in `screenshots`:
  `{"id","caption","session","name","account","at","url"}`.
- **screenshots**: a card of screenshots a session sent (several sent by
  the same session within two minutes make one card; `updated_at` moves
  when one joins). `session`, `name`.
- **pr**: a PR a session opened (see "PRs"): `text` is its title when
  found, `pr` the PR as it is now. It changes in place: a `chat` `update`
  (and a `pr` event) when its checks finish or it's merged or closed,
  never a new message. `session`, `name`, `space`.
- **event**: news the panel shows between messages (a watched session
  finished, a job done or blocked, an alert, a summary), `text` as the
  panel words it; `session`, `name`, `status`.

`id` is `h<line>`: stable, unique, in order. What the tower is writing
right now isn't a message yet: the state's `work` (its working line) and
`said` (what it is saying) show it until the answer lands.

## PRs

Every PR the tower's sessions open is a chip in the chat (a message of
`role` `pr`), the same on the Mac and every phone, from when it's found
(a `github.com/…/pull/N` link in what a session says at the end of a turn,
or what its `gh pr create` printed) until it's merged or closed. The Mac
asks GitHub about each open one every 30 s while its checks run, every
90 s otherwise. A PR:

```json
{
  "key": "eyzhub/eyz-mediaroom-serverless#1250",
  "repo": "eyzhub/eyz-mediaroom-serverless", "number": 1250,
  "url": "https://github.com/eyzhub/eyz-mediaroom-serverless/pull/1250",
  "title": "release: support tickets can open a new request beside an open one",
  "base": "main", "head": "dev", "head_sha": "0dcd66e…",
  "state": "open",                   // open | merged | closed
  "draft": false,
  "mergeable": "mergeable",          // mergeable | conflicting | unknown (GitHub still working it out)
  "checks": {"state": "failing",     // pending | passing | failing | none
             "failing": ["CI / lint"], "pending": [], "passed": 4, "total": 5},
  "session": "t58", "name": "b2c tickets mediaroom", "space": "b2c-tickets",
  "waiting": true,                   // its session ended its turn on it, or asked the user to merge
  "found_ms": 1794238300000, "checked_ms": 1794238390000,
  "closed_ms": null,
  "merged_by": null,                 // mac | phone:<device> | github, once merged
  "merge": {                         // whether the Merge button works now
    "can": true,
    "blocked": null,                 // why not at all: merged, closed, a draft, conflicts
    "needs_override": "1 check failing: CI / lint. Merge anyway only if you know the failure is unrelated.",
    "warnings": ["It merges into main: that ships to production."]
  }
}
```

Show "Waiting for you" when `waiting` and `state` is `open`. The Merge
button: disabled with `merge.blocked` as the reason when it's set. Tapping
it calls `…/confirm` and shows the answer (repo, number, `base`, the
checks, `method`, every warning); "Merge into <base>" then calls `…/merge`
with the token. When `needs_override` is set, that button first shows it
and asks once more ("Merge anyway: the failure is unrelated"), and only
then sends `override: true`. Afterwards the chip changes by itself (`pr`
event, chat `update`); the Mac tells the session ("The user merged PR #N
…", queued) and the brain. The brain never merges.

Example:

```
POST /v1/prs/eyzhub/eyz-mediaroom-serverless/1250/confirm   {}
→ 200 {"token":"9f2c…","expires_ms":1794238510000,"method":"merge","can":true,"blocked":null,
       "needs_override":null,"warnings":["It merges into main: that ships to production."],"pr":{…}}
POST /v1/prs/eyzhub/eyz-mediaroom-serverless/1250/merge     {"token":"9f2c…"}
→ 200 {"merged":true,"pr":{…,"state":"merged","merged_by":"phone:d_9fce5c6c"}}
event pr   {"op":"closed","pr":{…,"state":"merged"}}
event chat {"op":"update","message":{"id":"h1840","role":"pr",…,"pr":{…,"state":"merged"}}}
```

## Live updates: `GET /v1/events`

Server-Sent Events (`text/event-stream`), one long response. Use OkHttp's
`okhttp-sse` `EventSource` with the `Authorization` header; reconnect with
backoff (1, 2, 5, 10 s) when it drops. Every event has `id`, `event` and one
line of JSON `data`. A comment line `: ping` comes every 10 s.

| `event` | `data` |
|---|---|
| `hello` | `{"api":1,"device":"d_…","voice":bool}`, first |
| `state` | the whole state, first right after `hello`, then whenever it changes (at most 4 a second) |
| `history` | one new history entry (as in `/v1/history`), as it's written |
| `pr` | `{"op": "open"\|"update"\|"closed", "pr": {…}}`: a PR a session opened appeared, changed (checks, mergeable, waiting) or closed (merged or closed). |
| `question` | `{"op": "open"\|"update"\|"closed", "id", "question": {…}}`: a question set appeared, changed (recommendations arrived, the dialog moved to another tab, answering started, finished or stopped) or closed (the session moved on). |
| `behaviors` | a change: `{at, op, id, title, by, undid, diff}` (the chat also gets a `behavior` card with the diff) |
| `chat` | `{"op": "add"\|"update", "message": {…}}`: a message joined the thread (from the Mac or any phone), or one changed (a screenshot card grew). Messages the phone sent itself come back this way too. |
| `audio` | `{"id":"a17-9c2e1a0b4d3f5e6a","kind":"reply"\|"alert"\|"ack"\|"cue","text":"…","ms":1840,"url":"/v1/audio/a17-9c2e1a0b4d3f5e6a"}`: on voice streams for what the voice says, and on every stream of the phone a turn of its own is for (see "Where replies are heard") |
| `hush` | every stream: stop playing and drop everything queued |
| `pause` / `resume` | every stream: someone is talking at the Mac (hold playback, then go on) |

`?voice=1` makes this phone **the tower's voice** for as long as the stream
is open (see below). Without it, the stream is just the panel's feed.

## Voice

### Choice: whole recordings up, sentence clips down, all over HTTPS

- **Up: one WAV per take.** The phone records while the user holds the talk
  button (or until it hears a pause), then `POST /v1/voice` with the whole
  recording. Whisper on the Mac transcribes complete takes anyway (that's how
  the Mac's own press-to-talk works), so streaming the microphone would add
  a protocol and no speed: a 5-second take uploads over Wi-Fi in tens of
  milliseconds and transcribes in about half a second.
- **Down: one WAV clip per sentence, announced on the event stream.** The
  tower already speaks a sentence at a time (Kokoro makes the next while one
  plays). When a phone is the voice, each finished sentence (and the canned
  acknowledgement, and the alert bell) becomes a clip: an `audio` event
  tells the phone, which fetches `GET /v1/audio/{id}` and plays clips in the
  order the events came. The first words reach the phone as fast as they'd
  play on the Mac.
- Plain HTTPS requests and one SSE stream: no WebSocket, no RTP, no codecs to
  negotiate; it works through anything that carries HTTPS, it's easy to
  retry, and it reuses the same auth and TLS pin. WAV is large-ish
  (48 KB/s at 24 kHz) but trivial on a LAN, and needs no encoder on either
  side. (Opus/OGG can be added later behind `Accept` without changing the
  flow.)

### Speaking into the phone

```http
POST /v1/voice
Authorization: Bearer htk_…
Content-Type: audio/wav
Content-Length: 160044

<RIFF WAVE: 16-bit PCM (or 32-bit float), mono or stereo, 8 to 96 kHz; 16 kHz mono preferred>
```

```json
200 {"heard": "what's everyone doing", "sent": true}
200 {"heard": "", "sent": false}     // silence, noise, or a stop word ("stop": it hushes the tower)
415 {"error": {"code": "bad_audio", …}}
400 {"error": {"code": "bad_length", …}}   // under 0.2 s or over 120 s
```

**With files**: send `multipart/form-data` instead, the WAV as a part named
`audio`, and up to 6 file parts as for `/v1/ask`. The transcript gets
`[Image #N]` markers in front and the paths attached, as above;
the answer adds `"attachments": [paths]`. (A stop word with images is sent,
not taken as "stop".)

### Dictation: transcribe without sending

`POST /v1/transcribe` (or `POST /v1/voice?send=0`) takes the same WAV and
returns `200 {"heard": "…", "sent": false}`; nothing reaches the tower, and a
stop word is just text. It uses the same Whisper model and vocabulary (the
session names) as the panel's dictation. For **text while the user is still
talking**, do what the panel does: every second or so, post the recording so
far to `/v1/transcribe` and show what comes back, then post the whole take at
the end (to `/v1/voice` to send it, or `/v1/transcribe` to put it in the text
box). Keep one request in flight at a time: the Mac transcribes one
recording at a time, and a 30-second take takes about a second.

The Mac resamples to 16 kHz, transcribes with the same Whisper model and
vocabulary as the Mac's microphone, and hands the text to the tower as a
`[voice]` message, exactly as press-to-talk does. The call returns after the
transcription (about a second); the reply comes on the event stream. On
Android: `AudioRecord` at 16000 Hz, `CHANNEL_IN_MONO`, `ENCODING_PCM_16BIT`;
write a 44-byte WAV header in front.

### Where replies are heard: the phone's `audio_route`

Each paired phone has a setting, **`phone`** (the default) or **`mac`**: where
the replies to what *it* sends (asks, voice takes, approve/refuse) are heard.
Change it with `PUT /v1/device/audio`, in the Mac panel's Phone section, or
`tower phone route <device id> phone|mac`. It is kept on the Mac
(`devices.json`), so it survives restarts.

- **`phone`**: for a turn the phone started, its "Okay." and every sentence
  of the reply go to **that phone only**, as `audio` clips on **every
  stream it has open** (a plain `GET /v1/events` is enough; `voice=1` isn't
  needed), and **the Mac stays silent** for that turn. If the phone has no
  stream open, the clips are not played anywhere: they are **kept 15
  minutes** (the last 200) and listed by `GET /v1/audio?after=<last id>`, so
  the app can play what it missed when it reconnects, or skip it (the text
  is in the history either way).
- **`mac`**: the phone sends, the Mac speaks the reply as if it was asked on
  the Mac; the phone shows the text (history and `state.said`).
- Turns started on the Mac (its microphone, the panel, `tower ask`) and
  alerts: spoken on the Mac, unless a phone with route `phone` has a
  `voice=1` stream open, which takes the voice as before (a route-`mac`
  phone's voice stream doesn't).
- `hush`, `pause` and `resume` go to every open stream.
- **Voice on this Mac** (`voice_on_mac`; `tower mac-voice on`, or the
  panel's Settings > Voice): everything is spoken on the Mac and no phone
  gets an `audio` clip, whatever its route or `voice=1` stream. Off by
  default; `state.voice_on_mac` says which.

### The tower speaking on the phone

- While **any** stream with `voice=1` is open (of a phone whose route is
  `phone`), everything the tower says goes to the phones as `audio` clips
  and **the Mac stays silent**. When the last
  voice stream closes (the app disconnects, the API is turned off, or the
  network drops: noticed within about 30 s, when what was sent stays
  unacknowledged), the Mac speaks again. A clip already
  sent when the phone vanished is not replayed on the Mac.
- Clip kinds: `ack` (the canned "Okay." the moment a request reaches the
  tower), `reply`, `alert` (news nobody asked for), `cue` (the soft bell
  before an alert, `text` empty). Play them in event order, gapless.
- Clips are WAV, 24 kHz, mono, 16-bit, kept 15 minutes (or the last 200).
  A clip's id is opaque (in order, with a random part): take it from the
  `audio` event or `GET /v1/audio`. A clip for another phone's turn is
  `404` to this one, and so is any clip while the tower is muted. Fetch
  each with the bearer token; play from memory (`AudioTrack` in
  `MODE_STREAM`, or `MediaPlayer` with a `MediaDataSource`), since Android's
  players can't add headers to an HTTPS URL by themselves.
- `hush`: stop now and drop the queue (the user pressed hush, or started
  talking at the Mac). The phone should also call `POST /v1/hush` itself
  when the user starts a take on the phone, so the tower stops talking over
  them.
- `pause` / `resume`: someone is holding the Mac's talk key; hold playback.
- Keep the voice stream in a foreground service while the app is the voice,
  so Android doesn't kill it with the screen off.

## Versioning

Everything is under `/v1`. Additions (new keys, events, calls) don't change
the version; the app must ignore what it doesn't know. A breaking change gets
`/v2`, and `GET /v1/hello`'s `api` says what the tower speaks.
