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
tower phone disable     # stops the API (phones stay paired)
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
   minutes, one use, void after 5 wrong tries (each wrong try also waits a
   second). Opening a new code replaces the old one.
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
   most; request heads up to 16 KiB, bodies up to 24 MiB, recordings 0.2 to
   120 s.

What's left, honestly: anyone on the same network can see that a TLS server
runs on the port, and could try codes during the 5 minutes a code is open
(5 tries, 1 in 20 million). A phone that is stolen while paired holds a valid
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
- Times are Unix milliseconds (`*_ms`).
- Errors: `{"error": {"code": "<word>", "message": "<for a person>"}}` with
  status 400 (bad request), 401 `unauthorized` (missing, unknown or revoked
  token: re-pair), 403 (pairing), 404, 413/415 (audio), 429
  `too_many_failures`, 503 (the brain, `hc`, or the transcriber isn't there).
- Addresses (`t3`, `vps/t3`) are URL-encoded in paths: `vps%2Ft3`.

## Reading: what the panel shows

| Call | Returns |
|---|---|
| `GET /v1/hello` | `{"api":1,"tower":"0.1.0","device":{"id","name"},"voice":"af_bella","phone_is_voice":bool}`. Use it to check a stored token. |
| `GET /v1/state` | The daemon's state (below). Also pushed on the event stream. |
| `GET /v1/agents?all=1` | `tower agents --json`: `{"agents":[…],"unreachable":[…]}`; each agent has `address, account, machine, name, spoken, agent, status, doing, title, folder, where`. `all=1` adds plain shells. Slow-ish (asks Herdr, and other machines). |
| `GET /v1/spaces` | `tower spaces --json`: `{"focus": name or null, "spaces":[{"space","focus","agenda","unread","running_jobs","sessions":[{"address","name","status","account"}],"last_activity_ms"}]}` |
| `GET /v1/jobs?status=open\|finished\|all&session=t3&limit=50` | `{"jobs":[{"job","session","name","space","account","status","asked","summary","started_ms","updated_ms"}]}`, newest first. `status`: `working`, `blocked`, `done`, `failed`, `error`, `exited`. |
| `GET /v1/history?limit=100&session=t3&kind=summary,answer&before_ms=…` | `{"entries":[…]}`, oldest first, the latest `limit`. Page back with `before_ms` = the first entry's `at`. Summaries are `kind=summary`. |
| `GET /v1/accounts` | `{"default": name, "accounts":[{"name","email"}]}`, the default first (from the claude-accounts mod; names are not fixed: colour by name). |
| `GET /v1/screenshots?limit=30&session=t3` | `{"screenshots":[{"id","at","session","name","space","account","job","caption","path","url"}]}`, newest first (still on disk: kept 30 days). |
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
  "screenshots": [{"id": "1794160000000-login.png", "at": 1794160000000, "session": "t22", "name": "Claude in eyzflow", "space": "B2C support", "account": "default", "job": null, "caption": "The new login page", "path": "/Users/…/attachments/screenshots/1794160000000-login.png", "url": "/v1/screenshots/1794160000000-login.png"}]
}
```

`phase` (one word for the orb): `announcing`, `speaking`, `transcribing`,
`listening`, `thinking`, `blocked`, `armed`, `standby`, `idle`. Unknown keys
may be added later: ignore them.

## Acting: what the panel does

| Call | Body | Does |
|---|---|---|
| `POST /v1/ask` | `{"text": "what's everyone doing?"}` | Sends it to the tower like typing in the panel (it arrives as `[voice] …`). `202 {"sent": text}`. The canned "Okay." is played (on the phone when it's the voice). |
| `POST /v1/agents/{address}/answer` | `{"decision": "approve"\|"refuse", "note": "only the read"}` | Tells the tower, as the user would: "Approve what t22 is asking for. only the read". The tower reads the session's screen and answers it, as today. `202`. |
| `POST /v1/focus` | `{"space": "B2C support"}` or `{"space": null}` | Focuses a space (its news is spoken, others held) or clears focus. `200 {"focus": name or null}`; `404 no_space`. |
| `POST /v1/hush` | | Stops speaking now, and drops what's queued (the phone gets `hush`). |
| `POST /v1/mode` | `{"mode": "ptt"\|"handsfree"\|"off"}` | The Mac microphone's mode. |
| `DELETE /v1/device` | | Unpairs this phone (its token stops working). |

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
| `audio` | voice streams only: `{"id":"a17","kind":"reply"\|"alert"\|"ack"\|"cue","text":"…","ms":1840,"url":"/v1/audio/a17"}` |
| `hush` | voice streams only: stop playing and drop everything queued |
| `pause` / `resume` | voice streams only: someone is talking at the Mac (hold playback, then go on) |

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

The Mac resamples to 16 kHz, transcribes with the same Whisper model and
vocabulary as the Mac's microphone, and hands the text to the tower as a
`[voice]` message, exactly as press-to-talk does. The call returns after the
transcription (about a second); the reply comes on the event stream. On
Android: `AudioRecord` at 16000 Hz, `CHANNEL_IN_MONO`, `ENCODING_PCM_16BIT`;
write a 44-byte WAV header in front.

### The tower speaking on the phone

- While **any** stream with `voice=1` is open, everything the tower says goes
  to the phones as `audio` clips and **the Mac stays silent**. When the last
  voice stream closes (the app disconnects, or the network drops: noticed
  within ~10 s, on the next write), the Mac speaks again. A clip already
  sent when the phone vanished is not replayed on the Mac.
- Clip kinds: `ack` (the canned "Okay." the moment a request reaches the
  tower), `reply`, `alert` (news nobody asked for), `cue` (the soft bell
  before an alert, `text` empty). Play them in event order, gapless.
- Clips are WAV, 24 kHz, mono, 16-bit, kept 15 minutes (or the last 200).
  Fetch each with the bearer token; play from memory (`AudioTrack` in
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
