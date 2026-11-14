# The tower from the phone: 13 Nov 2026, 20:30–00:00

A review of one evening when the user worked with the tower only from the phone, away from the desk. It covers two spaces: `search-llm` (fine-tuning, plus MediaRoom search v2 in PRs #1392, #1393 and #1396) and `ticket-phishing-audit` (launched from the phone at 22:53; PRs #1398–#1401 and eyzflow #836–#838). All times are local (CET).

**Sources.** `~/.local/share/herder-tower/history.jsonl` (633 entries in the window), the 6 phone attachments (`attachments/*-phone-*`, all of them screenshots), `~/.local/state/herder-tower/decisions.json` (the last 50 closed decisions, 21:53 onwards; earlier cards are taken from the history's `decision` and `raised` lines), `queue.json`, the transcripts of the main brain and the `search-llm` and `ticket-phishing-audit` brains, and one reviewer's transcript.

## The evening in numbers

| | |
|---|---|
| Messages from the phone (voice or typed) | 19, between 20:48 and 22:53 |
| Cards closed between 20:30 and 00:40 | 57 |
| …that only offered **Show on Mac** (`blocked`) | 15: 12 memory-file edits, 3 "it shows something the tower can't read" |
| …that were a second card for the same merge | 6 (see F5) |
| …that were reviewer `rm node_modules` prompts sent to the user | 5 |
| "blocked" alert lines in the phone's chat | 74, against 19 answers to the user's own questions |
| Brain answers to the user | 19: median 441 characters, 6 over 100 words (about 40–60 s of speech) |
| Session replies relayed | 24: median 1,035 characters, 16 over 600 |
| Claude-folder prompts the tower approved itself after `ac85fba` (22:40) | 10 in about 50 minutes; each would have been a card before |
| PRs merged from the phone | #1392 (22:30), #836 (23:32), #1398 (23:32), #1399 (23:48) |
| PRs left for the Mac | #1400 and #837 (00:52–00:53), #838 and #1401 (00:58–01:00) |

## Timeline of friction points

**F1. Memory-note edits blocked sessions behind Show on Mac cards (20:30–22:40). Fixed since in `ac85fba`.**
- There were 12 cards titled `Asks to use Update: /Users/hassangilak/Library/Application Support/orca/claude-profiles/1f2995fa-2a8…`, all from t12v (mediaroom) and t12t (sooner). Ids include dff845c13bc01 (20:30), de132d7e78aa0 (20:48), d9d0690cd16f7 (22:06), d4cf938f58c97 (22:14), d75ec3c643c26 (22:15), d7ef30fba6bca (22:23) and d0c028e1fa555 (22:36).
- 20:47 screenshot: the only card in the queue is "blocked · 4 min" with a single **Show on Mac** button. The user typed "im not behind my desk. approve this for me".
- 22:14 screenshot: the same card for **sooner** at "20 min", with the header showing "3 need you · Mac out of reach". The sooner card dcfc8ca36ce61 waited 19.4 min (count 3) and was dismissed, never answered. The brain never saw it, because blocked cards didn't reach the brain.
- 22:15 "always approve this type of request…": the brain widened the behavior. At 22:17 the user sent a screenshot of the next card: "i still see it on the phone! havent you approved it?" It was a new prompt from the same session with the same title, so the phone couldn't show that it was new. The card came back four more times before the fix was installed at 22:40.
- After 22:40, 10 prompts were answered by the tower itself (`answered` lines 22:40–23:32) and no card appeared.

**F2. The user had to tell the tower three times that they were away (20:48, 20:51, 20:53).**
- The tower never knew. `brain::ask_as` (`src/brain.rs:199`) frames every message as `[voice] …`, whether it was spoken on the Mac or typed on the phone, so the source (`phone:d_0c7c288e`) never reaches the brain.
- The answers stayed desk-length: 124 words at 20:53, 149 at 21:58, 105 at 22:01, 105 at 22:06, 107 at 22:25 and 107 at 22:33. With `max_spoken_chars = 900` the longest were cut mid-way.
- The tower went mute at 22:40:11 (the `muted` file's mtime, written by `set_muted`) and was still mute the next morning. No session ran `tower mute`, and no record says who did, so it was most likely the phone's mute control.
- The user said "im back at my desk" at 01:03, from the Mac.

**F3. Two cards for one approval got opposite answers (20:48–21:17).**
- At 20:48:48 the brain raised df38fda13924b ("asks for your go to build its search fixes…"). At the same time the session's own end-of-turn wait card d6a0bdd7c7b23 asked for the same thing: "Still waiting on you: the host details, and approval to build my search layer."
- At 20:51:27 the user answered the first: "ok. put this feature behind the feature flag…". At 20:52:05 the second was answered **Not yet**, and that queued "Don't start anything yet" to t12v (a258).
- The session sorted it out at 21:16 from the order of the messages. Nothing in the tower noticed the contradiction.

**F4. A harmless prompt escalated, then expired before the phone answer arrived (21:16–21:22).**
- The brain chose `auto` twice for t12v's EXPLAIN script: "there is no rm in the command, the warning is the parser's". The tower overrode it both times with "it deletes or overwrites" (de4d808fbd003, d51f395f8f56c, both titled **"Allow this?"**).
- The dialog closed itself within a minute. The user's late answer produced the alert "Your reply to mediaroom didn't go in: it waited too long" at 21:22:21.

**F5. One merge produced two or three cards.**
- Besides each `merge` card, a session's wait or question about the same PR became a separate card:
  - df016f7c4bb6a "both are open and mergeable, waiting for you" (#1392)
  - d55873f73a53b "#836 is approved and waiting for you to merge"
  - dc3de4277974f "My PR #1398 is still waiting for your merge"
  - d90c5d75f0cba (#1399)
  - da1aaf68c8c43 "PR #1400 … still needs your merge"
  - db88776d8334d "Both releases are ready for you to merge"
- They fold into the merge card only when the message holds a PR **link** (`src/daemon/asks.rs:930–947` through `prs::links`). A bare `#1398` doesn't count.

**F6. The #1392 merge: asked about four times, 45 minutes on the card (21:45–22:30, d3ccc92853b4e).**
- 22:05 "ok you think we can merge the pr?" The brain said not yet and promised: "I'll tell you when PR 1393 is approved and the flag is in" (22:06:51).
- The flag landed at 22:13:00 (t12v's reply). The space brain's line about it was record-only at the `decisions` tell level, and the main brain had no turn between 22:06:50 and 22:15:03. **The promise wasn't kept**: the user asked "the merge is ready?" at 22:17:57.
- 22:16 screenshot: the merge card sits under "2 more waiting", behind the sooner Show on Mac card that is marked **Next**.
- 22:18:44 "if things are ready merge them". The brain declined: "I can't merge PRs myself… you can merge it into dev from there with one tap."
- 22:24 screenshot: the merge sheet shows "Checks: No checks", "⚠ No checks ran on it." and "⚠ Needs review: a database migration…". It has **no review row**, although the PR carried `review.line = "Review: agreed"`. The user asked: "havent you told me you checked it already?"
- 22:25 the brain explained that the 208 tests ran locally and that GitHub has no CI. The user merged from the phone at 22:30.

**F7. A merge went through a wait card's button, past the merge sheet and the reviewer (22:43).**
- d1d883df78ded: the user tapped the model-written choice "Merge #1396 into dev". The tower queued "Merge #1396 … now" to t12v (a268), and the session merged it itself on GitHub at 22:44:36.
- At that moment the PR's reviewer (t134) was still waiting on its own escalated approval prompt (dabf4f8995911).
- So at 22:18 the brain refused a spoken "merge them" ("the tower keeps merges for you"), yet 25 minutes later a button did exactly that, with none of the merge sheet's gates.

**F8. A wait card whose button didn't explain itself (22:22–22:23).**
- d732c7f273ced "Building the route needs your go and would be a separate PR after #1392" was answered with its choice "Wait for its answers first". A minute later the user asked "what answers it ls waiting for?"
- Other card titles carried no content:
  - **"For you to decide"** (d0ac8cbcd5c68, a markdown heading taken as the sentence)
  - **"Both of those need your OK."** (dfba9ca600404)
  - **"Allow this?"** (6 cards)
  - **"Needs you: it shows something the tower can't read"** (3)

**F9. The launch opened nothing and nobody was told (22:53).**
- The brain's first `tower launch "…"` failed: "name the repos in the goal's first line… no project folder fits". The brain had piped the output through `| grep -E '"(space|name|…)"'`, which dropped the error, and the tower kept no record of the failed attempt (no act, no history line).
- The brain found out only after `tower missions` and `tower acts` showed nothing and a `--dry-run` printed the reason. It launched again 16 s later. The user never saw the failure, so a dropped error elsewhere would go unnoticed too.

**F10. Reviewer prompts reached the user, and one stalled a merge for 55 minutes (22:41–00:59).**
- Review worktrees have no `node_modules`. Reviewers `ln -s` the main checkout's, then end with `rm node_modules && gh pr review --approve …`.
- The `rm` sends **the approval itself** to the user. There were 5 cards: d1f869cd02a8f (stopped), dabf4f8995911, dadd2b69f92a8, dce3b0d5978e9 and d18b08c66948d. Each time the brain said it was harmless and still had to escalate.
- dce3b0d5978e9 held #1400's review from 23:58 until the merge at 00:52. #1400's merge card showed "The review isn't done: its reviewer is still reading it", and it was merged at the Mac.

**F11. The weekly-limit failover showed up as alarms (23:08, 01:32).**
- The default account ran out at 23:08. The failover moved t138, t139 and the space's tower to orca within seconds, but first the user got:
  - two "is stuck on an error" alerts (t138, t139)
  - two "is now error" watches
  - a Show on Mac card dd26a84a06300 ("it shows something the tower can't read", which was the limit screen)
- The same happened to t12w at 01:32 (d82f9347aecdb). The main brain's own turn at 01:03:10 answered "You've hit your weekly limit…" before the failover re-asked it on orca, 22 s later.

**F12. Repeated status questions, and a false "Nothing needs you".**
- "where we are with the fine tunings?" was asked at 20:53 and again at 22:32. Both answers were over 100 words, even though the training state was a single number (step 900 of 2,500).
- 01:29 screenshot: "Nothing needs you" and no working session, while t12r's training ran in a background shell. The user asked for this to be fixed at 01:32; it is queued with t12w as a279.

**F13. Noise in the chat.**
- 74 `blocked` alert lines between 20:30 and 00:00:
  - 21 "waits for you: …" repeating a card's title
  - 13 bare "mediaroom is waiting for you"
  - 11 memory-edit lines
  - 20 exact repeats
- There were 19 answers to what the user had actually asked. Every card already appears in the queue, so these lines added nothing.

## What to change, quick wins first

| # | Change | Where | Effort | Fixes |
|---|---|---|---|---|
| 1 | **Tell the brain the message came from the phone.** Frame phone messages as `[phone] …` (or add "the user is away from the Mac") and add a rule in the brain's instructions: on the phone, answer in 1–2 sentences (≤40 words), offer more, never point at the Mac, and act on things the phone can't reach. Remember "away" until a message comes from the Mac. | `src/brain.rs:199` (`ask_as` already receives `source`); `assets/tower-core.md` | S | F2, F12 |
| 2 | **Readable card titles.** Add `Update` beside `Edit/Write/MultiEdit` so the card reads "Asks to edit lfm-license-confirmed.md". Make "Allow this?" name the command's first words. Reject wait-card titles under 5 words or that start with a reference ("Both of those", "For you to decide") and use the brain's one-line summary or the first bullet instead. | `src/daemon/asks.rs:2238` (`blocked_notice`), `src/questions.rs:639`, `src/questions.rs:1455` (`waits_for_user`) | XS–S | F1, F8 |
| 3 | **Reviewers don't `rm`.** The tower links `node_modules` into a review worktree when it makes one and removes the link when the review ends. The reviewer's instructions say not to touch it, and never to chain `rm` with `gh pr review`. | `src/daemon/reviews.rs` (worktree setup and teardown), `src/worktrees.rs`, the reviewer's prompt | S | F10 |
| 4 | **One card per merge.** Fold a wait or question card into the open merge card when it names `#N` of an open PR in the same space, not only a PR link. More generally, fold a session's wait card into an open raised decision from the same session about the same thing. | `src/daemon/asks.rs:930–947`, `src/prs.rs` (match `#N` against the space's open PRs) | S (merge) / M (general) | F3, F5 |
| 5 | **Usage-limit screens aren't errors.** When the failover is moving a session, or its screen shows the limit message, don't open a blocked card or "stuck on an error" alert; let the failover's one line speak. | `src/daemon/failover.rs`, `src/daemon/asks.rs` (blocked path), `src/daemon/watch.rs` (error alert) | S | F11 |
| 6 | **Merge sheet on the phone shows the review and the evidence.** Add a "Review: agreed (round 1) · newer commits not reviewed" row, show "No CI on GitHub" instead of "No checks", and show the session's own test evidence (`tower validate`) when there is any. | Android `ui/PrChip.kt` (confirm sheet), `ui/PrModel.kt`; the data is already in `pr.review` | S | F6 |
| 7 | **Say when a merge becomes ready.** When an open merge decision turns mergeable (review agreed after changes, the flag commit lands, blocked becomes can), say one line even at the `decisions` tell level. Add a rule: the brain never says "I'll tell you when…" without a mechanism behind it (`tower hold --reason`, a watch). | `src/daemon/decisions.rs` (merge decision updates), `assets/tower-core.md` | S–M | F6 |
| 8 | **Log failures and mutes.** Write a failed `tower launch` to the history as an act line, and print its error as JSON on stdout when piped so filters keep it. Write mute on/off to the history with who did it (phone device, Mac, CLI). Add a brain rule: don't `grep` the output of `tower` commands that change things. | `src/launch.rs` (`cmd`), `src/daemon/tts.rs` (`apply_mute`), `src/daemon/lan/mod.rs:1625` | XS–S | F9, F2 |
| 9 | **Cards in the queue, not again in the chat.** Don't write a chat alert for a wait or blocked state that has a card; keep alerts for what has no card (errors, finished requests). | `src/chat.rs`, `src/daemon/watch.rs:1092/1294`, Android `ui/ChatModel.kt` | S | F13 |
| 10 | **"Asked again" on a card that comes back.** When the same session reopens a card with the same key within minutes of an answer, say "asked again (3rd edit in 10 min)" on the card and in the brain's note. | `src/decisions.rs` (`blocked`, keyed `blocked:<space>:<session>`) | S | F1 |
| 11 | **Escalate on the command, not on words.** `must_escalate` matches "delete/drop/overwrite/rm" anywhere in the dialog text. Match the command's own tokens instead (a `rm`/`DROP` that runs, not one in a heredoc or SQL string), and let the brain's `auto` stand when the only match is in prose. Show the expiry on the card ("closes in 50 s"). | `src/questions.rs` (~1900–1995, `must_escalate`) | M | F4 |
| 12 | **Merges follow the user's words, through one gate.** Add `tower merge <pr> --said "<the user's words>"` for the brain, valid only in the turn where the user said to merge, with the same gates as the phone's sheet (blocked, needs-override, review). Stop wait-card choices from telling a session to merge. Either way, the merge goes through the tower. | `src/prs.rs` (merge), `src/main.rs` (command), `src/choices.rs` (no "merge" options), `assets/behaviors/approvals-need-a-yes.md` | M | F6, F7 |
| 13 | **Progress you can glance at.** On the phone's space card, show the session's running job and its last progress line ("training step 900/2,500, next check at 1,000"), so a status question isn't needed. | t12w's a279 (already queued); `src/daemon/watch.rs`, Android `ui/MissionScreen.kt` | M–L | F12 |

## Notes on the known pain points

- **Memory-file prompts with only Show on Mac.** Confirmed: 12 cards, the longest waiting 19.4 minutes. Fixed by `ac85fba` and working live (10 prompts answered by the tower after 22:40). The other 3 Show on Mac cards (F11) still happen.
- **A merge card the user couldn't act on from the phone.** No merge was actually blocked on the phone: four were merged there. The friction was elsewhere:
  - The phone's notification for a merge has no action (`DecisionNotices.kt:256`), so it has to be opened in the app.
  - #1392's card sat behind a Show on Mac card (F6).
  - The sheet's warnings contradicted the brain (F6).
  - #1400's review stalled on a reviewer `rm` prompt, so it was merged at the Mac (F10).
- **"have you approved it?"** The tower had approved. Each new memory edit opened a new card with the same title (F1, item 10).
- **"if things are ready merge them" declined.** Confirmed at 22:18:47. The same merge power was reachable through a wait card's button 25 minutes later (F7, item 12).
- **A launch that opened nothing without saying so.** Confirmed at 22:53:07. The error was printed, but the brain's filter dropped it and nothing was recorded (F9, item 8).
- **Replies too long to be heard.** Confirmed: six answers over 100 words, session replies with a median of 1,035 characters, and a mute at 22:40 that no one recorded (F2, items 1 and 8).
- **Cards titled with raw file paths.** The cause is one missing match arm: `Update` isn't treated like `Edit` in `blocked_notice` (item 2).
