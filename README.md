# herder-tower

A voice control tower for [Herdr](https://herdr.dev). Talk to it, and it
steers every Claude Code and Codex session you run in Herdr, on this Mac and
on every machine added with `hc machine add`. It answers out loud.

<p align="center">
  <img src="docs/screenshots/orb.png" width="420" alt="The orb: calm when ready, cyan while it listens, periwinkle while it replies">
</p>

<table>
  <tr>
    <td align="center"><img src="docs/screenshots/chat.png" width="300" alt="Chat with the tower, a picture attached as [Image #1]"><br><sub>Chat: type or talk; attach files as tokens</sub></td>
    <td align="center"><img src="docs/screenshots/work.png" width="300" alt="The working line: Delegating to web, 15s"><br><sub>The brain at work, from its own hooks</sub></td>
  </tr>
  <tr>
    <td align="center"><img src="docs/screenshots/activity.png" width="300" alt="Activity: running and finished jobs by session"><br><sub>Activity: jobs running and done</sub></td>
    <td align="center"><img src="docs/screenshots/spaces.png" width="300" alt="Spaces: one focused, the others' news held"><br><sub>Spaces: focus one, hold the rest</sub></td>
  </tr>
</table>

Everything that listens and speaks runs locally, in one Rust binary:

| Part | What | How |
|---|---|---|
| Ears | Whisper large-v3-turbo (q5) | [whisper-rs](https://crates.io/crates/whisper-rs) → whisper.cpp on Metal, in process |
| Mouth | Kokoro-82M | [loqui](https://crates.io/crates/loqui) → ONNX Runtime with a pure-Rust G2P (no espeak, no Python) |
| Microphone, speakers | | cpal, rodio |
| Radar | every terminal's status | `hc events` here, `hc exec --on <machine> hc ls` there |
| Brain | a Claude Code (or Codex) session named `tower` | drives the others with `hc` |
| Talk key | Control, anywhere on the Mac | a listen-only CGEventTap in the daemon |
| Window | `tower orb` ([GPUI](https://gpui.rs), Zed's UI framework) | one floating orb: how it moves says what the tower is doing |

Measured on an M5 Pro: Kokoro makes speech about 8× faster than real time
(first sentence in ~0.2 s); Whisper turbo transcribes a 4-second request in
~0.5 s.

## Use

```sh
tower up            # the orb, the voice (Herdr terminal tower-voice) and the brain (terminal tower)
tower up --show     # ... and show the brain in a Herdr tab
tower up --no-orb   # ... without the orb
tower down          # stop the voice and the brain (the orb stays, faded: click it to start again)
tower orb           # just the orb
```

Then:

- **Tap Control** (alone, anywhere on the Mac): speak; a pause ends the take,
  another tap sends it at once. **Hold Control**: talk while holding, let go
  to send. Control with another key (ctrl-c, ctrl-r, ...) is left alone.
  Talking over the tower hushes it.
- **The orb** floats over every window, on every Space: a small cup of
  liquid that turns, wobbles and boils. Its mood follows the tower: calm and
  pale blue when ready, icy cyan with a breathing halo while it listens to
  you, periwinkle with a halo that throbs on each beat while it replies, lively and
  spinning while it understands, its opening turned up while it thinks, a
  quicker beat while it speaks, faded when off, breathing in standby, with a
  red dot when an agent needs you. It reaches for the pointer, gives where you
  press it and sloshes when dragged. Hover for the last thing heard or said;
  click for the panel (below); right-click to show the tower's own terminal; drag it
  anywhere (it comes back there next time). ⌘⇧K, from any app, shows or hides
  it. It is its own small window (no Dock icon), so it works with Herdr closed.
- ⌘⇧Space also talks and ⌘⇧Esc hushes, from any app (the orb handles them).
- `tower mode handsfree`: always listening; start with "Tower, ..."; say
  "stop" or "hush" to cut it off. `tower mode ptt` is the default.

The orb is the liquid orb from the orbs prototype (`eyz/orbs`, `index.html`
with its default settings) rewritten in Rust: its liquid-drop physics in
`src/orb/liquid.rs` and `src/orb/sim.rs`, its raymarched shell as a Metal
shader in `src/orb/orb.metal` (GPUI draws everything else on top). A light
screen gets the prototype's light look (navy on beige), a dark one its glow
look (blue on black); either backdrop becomes transparency.
`HERDER_TOWER_ORB_LOOK=light` or `glow` picks one. How each phase moves it
is `Drive` in `src/orb/render.rs`.

The orb used to be a Herdr mod showing the `orb-*.png` artwork; its source
is still in `mod/tower`, but `tower install` no longer puts it in Herdr, and
takes the keys it added (⌘⇧K, ⌘⇧Space, ⌘⇧Esc) out of Herdr's `keys.json`:
the orb has them now.

### The panel

Click the orb for a glass chat with the tower (built with
[GPUI Kit](https://gpui-kit.com) on macOS's own blur): your messages on the
right, its answers on the left, what the sessions reported in between.
Type in the pill at the bottom: Enter sends, Shift+Enter starts a new line,
Esc closes. While you talk (Control or ⌘⇧Space), what you say appears in it as you say it, transcribed every second or so, then the final text, which goes to the brain as any spoken request.

Attachments work as in Claude Code and Codex: paste a screenshot or a file
copied in Finder, drop files on the panel, or pick them with +. Each becomes a
token where you were typing, `[Image #1]`, `[Video #2]`, `[File #3: name]`;
delete the token and the file goes with it. The message reaches the brain like
a spoken request, with the files' paths in token order (copied to
`~/.local/share/herder-tower/attachments` first, so they stay put).

The jobs button (by the status) shows what the tower handed out, in progress
and finished, by space or session. It all lives in
`~/.local/share/herder-tower/history.jsonl` (append-only): `tower watch`
starts a job, the watch report ends it, the brain adds the outcome
(`tower job done`) and session summaries (`tower summary`).

Each Claude session, job and history row carries a small badge naming the
Claude account it runs on (the `account` the radar reports per agent); Codex
sessions have none. The panel has no list of accounts of its own: it shows
whatever name the data carries, outlines the default account (the first
`tower accounts` lists, re-read every 30 s) and fills any other account with a
colour picked from its name, so a new account needs no change here. A space shows its
account when all its Claude sessions share one, and "mixed" when they differ.

Things to say: "What's everyone doing?", "Tell the eyzflow session to run
the tests and fix what fails", "What is Codex on the VPS asking?", "Approve
it", "Start a Claude session in sooner-app-web and have it look at the login
bug" (add "on the <name> account" for another account). The brain hands off
work (`tower start` for new sessions), calls `tower watch`, and tells you when the
session finishes. When any agent anywhere needs you, the radar says so.

### Claude accounts

The tower knows nothing about your accounts itself: it asks the
claude-accounts mod (the `cca` command, in the Herdr header) each time.
`cca --porcelain` lists the accounts, the first being the default (cca's
`default`: `~/.claude`, with `CLAUDE_CONFIG_DIR` unset). `cca --porcelain
<pid>` names the account a running `claude` uses, from the
`CLAUDE_CONFIG_DIR` in its environment. Add an account with `cca add <name>`
and the tower can use it straight away.

```sh
tower accounts                      # the accounts cca knows; * is the default
tower start --name fix-login --cwd ~/w/app "look at the login bug"
tower start --account <name> --name fix-login --cwd ~/w/app "..."
tower start --on vps --name x "..." # uses cca on that machine, if it has it
```

`tower start` runs `hc run --bg --outlive-parent ... -- cca <account> -n
<name> "<task>"`: cca sets (or unsets) `CLAUDE_CONFIG_DIR` and `CCA_ACCOUNT`
and starts claude. It records a job in the history with the account, and
watches the session (`--no-watch` not to). An account cca doesn't know is an
error that lists the ones it does. Without the mod, sessions start as plain
`claude` and accounts show as unknown.

Every Claude session on this Mac carries its account: `tower agents`,
`tower spaces`, `tower history` and jobs, and the `agents` in the daemon's
`state.json` (the orb's panel). It's the account's name as cca has it, or
`null` for Codex, other machines, and sessions it can't tell. A process's
account never changes, so it's looked up once per pid and kept in
`~/.local/state/herder-tower/accounts.json`.

From a shell (or a Raycast/skhd hotkey outside Herdr):

```sh
tower talk | tower hush | tower mode handsfree
tower ask "what's waiting for me?"   # typed instead of spoken
tower say "hello"                    # speak anything
tower agents                         # every agent on every machine, with its account (JSON when piped)
tower accounts                       # the Claude accounts (from cca)
tower start --name n --cwd d "task"  # a new Claude session on an account (--account), watched
tower history [--jobs open|finished] [--session t3]   # what the tower has done (JSON when piped)
tower job start|done|failed <t> "…"  # record a job (the brain does this)
tower summary <t> "…"                # record what a session has done (the brain does this)
tower status                         # the daemon's state
tower doctor                         # check the setup
tower selftest                       # Kokoro speaks, Whisper listens: no mic needed
```

## Install

```sh
brew install cmake          # whisper.cpp builds with it
cargo build --release       # GPUI compiles its Metal shaders at launch: no Metal Toolchain needed
./target/release/tower install
tower up
```

`install` puts the binary in `~/.local/bin/tower`, writes
`~/.config/herder-tower/config.toml` (voice, Whisper model, wake words,
alerts) and the brain's folder `~/.config/herder-tower/brain/` (its
instructions and Claude Code hooks). The first `tower up`
downloads Kokoro (~330 MB) to `~/.cache/loqui`. The Whisper model is
`~/.local/share/herder-tower/models/ggml-large-v3-turbo-q5_0.bin` (from
`huggingface.co/ggerganov/whisper.cpp`). macOS asks once for microphone access
for **Herdr Core**, because the voice runs in a Herdr terminal.

## How it fits together

```
 you ─ mic ─▶ tower-voice (Rust daemon) ─ hc send "[voice] …" ─▶ tower (Claude Code)
                 ▲  │                                              │  hc ls/read/send/keys
                 │  └─ state.json ─▶ tower orb (its own window)    ▼
     Stop hook ──┘◀── its answer ──────────────────────────── every agent, every machine
     (tower hook claude)                    radar: hc events / hc exec --on <m> hc ls
```

- The brain's last message each turn goes through its Stop hook to the
  daemon, which strips markdown and speaks it a sentence at a time.
- `tower watch <t>` (the brain calls it after handing off work) makes the
  daemon send the brain a `[watch]` message when that session stops working,
  is blocked, or sits on a first-run "trust this folder?" question.
  Watches survive restarts: `~/.local/state/herder-tower/watches.json` keeps
  them, the last status of every agent, and reports the brain hasn't taken
  yet. On start the voice compares that with what Herdr says now and reports
  what finished, blocked or ended while it was down, for watched sessions and
  for sessions with an open job. A watch leaves the file in the same write
  that queues its report, so each finish is told once, and a report waits
  (said out loud once) until the brain is there to take it.
- Codex as the brain: `tower up --brain codex` (it uses Codex's `notify`
  instead of a hook, and runs with `--sandbox danger-full-access
  --ask-for-approval on-request` because `hc` talks to herdr-core over a Unix
  socket).

## Limits

- No acoustic echo cancellation yet: in hands-free mode the tower ignores
  what it hears while it talks, except a stop word. A loud speaker can
  drown a spoken "stop"; press ⌘⇧Esc or Talk instead, or use headphones.
  (macOS voice processing via AVAudioEngine would fix this.)
- The Control key needs Input Monitoring for Herdr Core (System Settings >
  Privacy & Security). Set `talk_key = "right_ctrl"` (or `off`) in
  config.toml if plain Control taps get in your way.
- Other machines are polled every 15 s, so their alerts lag a little.
