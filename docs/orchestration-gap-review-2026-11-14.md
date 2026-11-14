# Orchestration gap review: the tower against "minimal human handoff"

14 Nov 2026. The tower is set against a guide to running agents with as little human handoff as possible:

- a strong orchestrator;
- narrow workers;
- durable state with resume;
- self-correction before escalation;
- structured handoffs;
- retries with backoff;
- full observability;
- humans only at high-stakes checkpoints;
- a measure of end-to-end completion without human fixes.

Budget and cost caps are left out, at the user's request.

Evidence comes from the last 24 hours: 13 Nov 06:31 to 14 Nov 06:31 CET. Sources:

- `history.jsonl` (3,661 entries);
- `agendas.json` (156 agendas, 154 tasks);
- `decisions.json`;
- the record of tonight's fixes on `main`.

Code references are to `main` at bc5c283.

## Baseline: how often a request finishes with no human fix

**15 of 44 requests (34%) finished with no human step at all. Another 4 needed only checkpoint approvals, making 19 of 44 (43%). The other 25 (57%) needed the user to step in mid-task.**

The user was asked 66 times across 21 of the 44 requests (48%):

- 37 of the asks were checkpoints: merge, deploy, push, secrets, delete, data.
- 29 were asks to unblock work.

During those requests, the user typed or answered into the working sessions 62 times.

| Last 24 hours | Count |
|---|---|
| Requests of the user's (`origin: user` agendas) | 53 |
| … minus double registrations (a191 = a189, a311 = a310) and bare replies (a186, a217, a221, a280, a314, a320) | **44 new requests** |
| Finished (agenda `done`) | 43 (a247 still open after 12 h) |
| Finished with no human step | **15 (34%)**: a203 a205 a210 a218 a242 a243 a249 a252 a281 a290 a296 a297 a310 a322 a323 |
| … plus those with only checkpoint approvals (merge, deploy, push, secrets) | **19 (43%)**: adds a188 a193 a194 a222 |
| Needed a fix: the user typed into a task's session, answered an unblocking card, or replied | **25 (57%)** |
| Requests where the user was asked anything (a card) | 21 (48%); 66 cards in total (29 unblock, 37 checkpoint) |
| Every distinct card in the window | 211 (plus 39 answered by the tower itself, `answered … auto`) |
| "Needs you" alerts | 268 (only 216 distinct texts) |
| Every human input | 193: 63 typed to the tower on the Mac, 27 from the phone, 71 card answers relayed into sessions, 32 typed straight into terminals |

### How the baseline was counted

- **The request's window** runs from `request_ms` to `closed_ms` (or now).
- **A human step** is any of these, taken in the window into a session that holds one of the request's tasks:
  - a history `request` entry with source `answer:<session>` (a card answered and relayed) or `terminal:<session>` (typed in its terminal);
  - a `decision`/`raised` card for that session (split into checkpoint and unblock by its `why`);
  - a bare reply of the user's to the same space.

### Caveats on the count

1. **There is no link from a card or a typed answer to the request it served.** The count joins them by session and time, so a session working on two requests at once counts a human step against both.
2. **"Finished" is the ledger's word, and it was wrong at least twice.** a252 and a297 count as clean, but both were marked verified by mistake (see G3). The real clean rate is lower than 34%.
3. **Not every human step is a fix.** Many of the 62 are the user steering the design, for example "Do not enable inline mode yet" (13 21:51, t12v). A real metric should tell "steered" from "unblocked" (see the metric design below).

Even so, the shape is clear. Most requests that needed help needed it because a session stopped to ask, not because the work failed.

## Where the tower already fits

- **Orchestrator owns routing, state and failure detection.**
  - `tower dispatch` routes deterministically: a named session, else the live session in that folder or worktree, else a new one. Dependencies are ordered with `after` (`src/dispatch.rs:179,226`).
  - `agenda::assess` (`src/agenda.rs:1773`) runs every 20 s. It finds stalls (25 min without progress), instructions never taken (12 min), idle without a word (2 min), errors and exits.
  - Background jobs hold judging while they run (`src/daemon/running.rs:118`).
- **Narrow workers.**
  - PR reviewers: critical issues only, never edit, 3 rounds (`src/reviews.rs`).
  - The planner, which can't launch (`src/plans.rs`).
  - A brain per space (`src/brains.rs`).
  - One-shot headless judges: `covered`, `choices`, `goals`.
- **Durable state.**
  - Every store is written atomically to disk (`agendas.json`, `queue.json`, `decisions.json`, `questions.json`, `watches.json`, `failover.json`, …).
  - The send queue claims an item on disk before typing it, so a restart doesn't type it twice (`src/daemon/sendq.rs:6-15`).
  - Failover resumes the same conversation on another account and tells it to carry on (`src/failover.rs:477`).
  - Spaces reopen after a restart: "Reopened after the restart: … each resumed its own conversation" (13 12:51:16).
- **Self-correction in places.**
  - The reviewer loop.
  - `tower validate` checks with evidence, which gate auto-merge (`src/plans.rs:797`, `src/daemon/automerge.rs`).
  - A failed task is retried once by itself before it becomes a card (`src/daemon/agendas.rs:288`).
  - The brain answers routine questions under the 90/10 rule (`questions::decide`, `src/questions.rs:2539`). It answered 39 tonight.
- **Humans at checkpoints.**
  - Merges, deploys, secrets and deletes always reach the user (`must_escalate`).
  - Auto-merge is off by default and narrow when on.
  - Cards are deduplicated by key: `ask:`, `merge:`, `raise:`, `blocked:`, `failed:` (`src/decisions.rs:620-645`).
  - Holds wake on their PR (`Book::hold/wake`).
  - Answers are typed back into the session for it, so the user never needs the terminal.
- **Observability.** Everything lands in `history.jsonl` with a kind and a source. Each task keeps up to 60 steps of state history with the reason for each (`src/agenda.rs:299`). That is what made this review possible.

## Gaps, with evidence

**G1. No measure of completion without help.**
- Nothing counts it; the only tally is "2/3 verified" (`Agenda::tally`, `src/agenda.rs:518`).
- The data to compute it is there but unlinked. A card carries a `task` only for the failed kind. A typed answer carries a session, not a task. Only 50 closed decisions are kept (`KEEP_CLOSED`, `src/decisions.rs:48`), against 211 cards in 24 hours.
- The baseline above had to be reconstructed by joining on session and time.

**G2. Handoffs are free text, both ways.**
- **Out.** The brain writes each instruction, and it is pasted verbatim (`src/dispatch.rs:131,358`). The only rule is `"instruction":"<complete and self-contained>"` (`assets/tower-core.md:71`). Tonight's 154 instructions had a median of 294 characters and a maximum of 3,587. Some carry an improvised "BRIEF FOR THE EYZFLOW SESSION / Goal / Context" (a282.3). Others end in asks made up on the spot, such as "Report the numbers with evidence, verified vs not verified" (a259.1). None says what was done before, what is uncertain, or the exact next action.
- **Back.** Sessions are told no report format. The tower reads the Stop hook's last message, and the brain is told "[watch] t12v (mediaroom) is now done. You had asked it: … Read its screen and tell me what happened, briefly." (`src/daemon/watch.rs:1306`; for example 13 22:06:09). So every handoff back is a screen-read by the brain.

**G3. "Verified" usually means a keyword, or that the user did it.** Of 151 tasks verified in the window:

| How the task was verified | Count |
|---|---|
| A keyword in the session's last message ("its last message says 'merged'/'passed'/'deployed' and names …", `agenda::judge`, `src/agenda.rs:2078`) | 75 |
| "its turn ended; the user worked it in it" | 42 |
| The brain checked it | 27 |
| By hand | 7 |

- **a292.1** was verified at 14 02:12:55, the same second its turn ended, because the last message said "deployed". Its task was a stuck wait for a deploy.
- **The wrong task gets settled.** `tower job done <session>` verifies the session's *latest unfinished* task, not a named one (`Ledger::judged`, `src/agenda.rs:1184`). Twice tonight that settled the wrong task:
  - a252.2: "marked verified by mistake: the merge/install job done settled it, but the 5-hour-row work had only just started on t12w" (13 19:35:19).
  - a297.1: "marked verified by mistake: the job done for t13z's merge-card description work settled it" (14 03:04:01).
- **Claimed done, then back at work.** 10 tasks claimed done and then went back to work: a219.1, a247.1, a257.1, a257.2, a259.3, a272.1, a272.2, a282.3, a302.1, a314.2.

**G4. No evaluator, and no alternate path before escalation.**
- A first failure is retried with the same text, prefixed "That didn't work ({why}). Try it once more:" (`src/daemon/agendas.rs:307`). A second failure is a card for the user.
- No critic compares the result with the instruction or the done-when before "verified".
- No confidence is kept.
- No other route is tried before asking the user: another session, a fresh worktree, or the brain re-planning.

**G5. Asks are counted per flicker, not per question.**
- a247.1 went from blocked to done-claimed and back about 20 times between 13 22:51 and 22:54, on one unchanged "2 questions: Dev deploy, Secret keys".
- The same session raised "search-llm › mediaroom is waiting for you" 13 times and "asks to use Update: …" 11 times (t12v).
- Merge card d86e738a26472 (#841) was raised 5 times.
- 268 "needs you" alerts went out against 216 distinct texts.
- Dedupe works on cards (`count` goes up), but the task state and the alert stream flicker with every look.

**G6. Failure detection by heuristics that misfire, and they reach the user.**
- 81 job alerts in the window: t13q 24, t13y 16, t13z 13.
  - Many said "job ended … (ran 0 min)" for foreground helpers (t13z at 14 02:10:24, t13y at 02:10:40).
  - Others flagged "may be stuck" on purposeful waits:
    - t138's `until … vercel ls` poll after 70 min (02:10:09);
    - t12r's post-training waiter after 48 min;
    - t7k's Postgres in Docker, "stuck 12 h 51 min", which is a server.
- It took six fixes in one night to tame it (54e4c97, 35219b5, 85ed49b, 1d14a06, 0fadf11, a06dd38).
- A job's news goes to the user as an alert, when the session's brain could judge it first.
- Questions were lost, not just noisy. The audit behind 96bab85 found 79 turn ends asking the user in 24 hours, 8 of which produced nothing from the tower. Each was read as a question and then dropped: no deadline while waiting on the brain, "asks nothing" closed it without a trace, or the turn ended while the tower was down.

**G7. Resume means "send the same instruction again".**
- `requeue` and the re-prompt send the instruction as first written (`src/agenda.rs:1159`). No checkpoint of what was done is kept on the task.
- A task that comes back after a stall, a failover or a restart gets "Carry on … Your last instruction was: …". The session has to rebuild where it was from its own conversation. If that conversation is gone, the work is gone with it.

**G8. Retries without backoff or a cap.**
- A failed send is retried on every look, with `tries += 1`, no backoff and no limit. It is only logged on the first try and every 20th (`src/daemon/sendq.rs:294,1606-1613`).
- Headless calls (ticket, covered) don't retry; they fail open.
- No harm from this showed up in the record tonight (queued 139, delivered 138). It is a robustness gap, not a felt one.

**G9. No correlation id across a request's life.**
- Request entry, agenda, task, queue id, card, PR, review and receipt are linked only in parts (`Task.queue_ids`, `Decision.task` for failed cards, `agenda` on receipts).
- Every question in this review ("which request was this card for?") had to be answered by time windows.

## Close look 1: a structured handoff

Keep it readable: the user reads these in terminals and on cards. The tower renders the format and parses it back; neither side writes JSON by hand.

### Out: tower to session

`tower dispatch` tasks get optional fields next to `instruction`:

- `goal`
- `done_when`
- `so_far`
- `constraints`
- `uncertain`
- `next`

The brain is asked to fill them (`assets/tower-core.md:71`).

`dispatch.rs` renders them in a fixed order. When only `instruction` is given, it goes out as it does today:

```
[tower] Task a282.3 for request a282 · space sooner-chatbot-answers
Goal: a Knowledge gaps section in Studio for the chats the bot couldn't answer
Done when: the section lists unanswered chats on dev; PR into dev; checked in the browser
So far: MediaRoom (t13q) owns the backend contract; #839 and #842 are merged into dev
Constraints: branch from dev, PRs into dev only; dev port 3031; never sign in for the user
Uncertain: the contract's field names; ask t13q, not the user
Next: rebase onto origin/dev, then build the list view against the contract
Report back: end your turn with a Handoff block (status, done, evidence, left, needs).
```

The same renderer serves the other paths:

- **Launch** (`src/launch.rs:699`): it already has goal, done-when and checks.
- **Retries:** `so_far` holds the failure reason. `next` holds a different approach, not the same text again.
- **Failover's carry-on.**
- **Requeue:** with the last Handoff as `so_far`, this is the checkpoint G7 lacks.

### Back: session to tower

A session ends its turn with:

```
Handoff a282.3
Status: done | partial | blocked | failed
Done: built the list view; PR eyzhub/eyzflow#840 into dev
Evidence: npm test 42 passed; screenshot shown in the tower
Left: the Assign button (waits on t13q's contract)
Uncertain: whether "handed over" chats belong in the list
Needs: nothing from the user
Next: wire Assign once t13q sends the contract
```

The parser goes in `asks::finished` and `agenda::judge`:

| Status | What the tower does |
|---|---|
| `done` with Evidence | Verified, with the evidence kept on the task |
| `done` without Evidence | DoneClaimed: the brain checks |
| `partial` | Stays running. The block is the task's checkpoint, and `next` is what a resume sends |
| `blocked` | One card whose title is the `Needs` line, so there is no screen-scraping (G5) |
| `failed` | The alternate-path retry (G4) |

- **No block:** the tower falls back to today's keyword and brain check, so old sessions and Codex keep working.
- **The brain's `[watch]` note** carries the parsed block instead of "Read its screen" (`src/daemon/watch.rs:1306`).
- **Session briefs:** `introduce` and the worktree brief tell every session the format once.

## Close look 2: completion without intervention, in the panel

### Record it as it happens, not afterwards

- **A field on the agenda:** `helped: Vec<Help { at_ms, kind, task, ref }>` in `src/agenda.rs`. `kind` is one of `typed`, `answered`, `approved`, `reasked` or `fixed_after_done`.
- **Where each one is written:**
  - `typed`: terminal input seen on a session with an unfinished task (`src/daemon/agendas.rs`, where `terminal`-origin input is handled today).
  - `answered`: a relayed card answer (`src/daemon/asks.rs`).
  - `approved`: a user-answered card whose `why` is a checkpoint, or `answered` when it is an unblock (`src/daemon/decisions.rs`, on answer).
  - `reasked`: a follow-up request to the same space within the agenda's life (`register`'s follow-up check, `src/agenda.rs:1107`).
  - `fixed_after_done`: a new request naming the same work within 24 h of done.
- **The links this needs** (also G9): the card gets `task` and `agenda` when raised from a session with an unfinished task, `Decision.task` for every kind. Answers typed through the tower carry the same.
- **Classified per request:**
  - *on its own*: no help;
  - *with your approvals only*: checkpoint `approved` only;
  - *with your help*: anything else;
  - *steered*: a `typed` the brain marks as new direction, not an unblock. It is counted apart so design decisions don't read as failures.

### Shown on the panel

- **The home header,** under "Nothing needs you · 2 working" (`src/orb/panel/home.rs`):
  `Last 24 h: 15 of 44 done on their own · 4 more with only your approvals · you were asked 66 times`
  Tapping it opens the list of requests with their help steps, each linking to its card or typed answer.
- **Each space's view,** the same line for its requests (`src/orb/panel/space.rs`).
- **The phone,** the same numbers in `state.json` / `/v1/missions` (`src/daemon/lan/quiet.rs:683`).
- **`tower stats [--since 24h] [--space <s>]`,** for the brain and for tests. It reproduces this review's numbers from the files.
- **Kept long enough to compute:** `KEEP_CLOSED` (50 closed cards) is too small for a day of 211 cards. Either keep 7 days of closed cards, or compute from `history.jsonl` decision entries with the new `task` field.

## Ranked fixes (quick wins first)

| # | Fix | Where | Effort | Evidence |
|---|---|---|---|---|
| 1 | `tower job done\|failed` names the task (`--task a252.2`). With more than one unfinished task on the session and no `--task`, refuse and list them | `Ledger::judged` `src/agenda.rs:1184`; `agenda_judged` `src/main.rs:2290`; tower-core.md | S | a252.2 (13 19:35), a297.1 (14 03:04) |
| 2 | One ask per question. While the same question set is still asked, a task stays Blocked (no flip to done-claimed). One alert per card, not per look | `agenda::assess`/`judge` `src/agenda.rs:1773,2078`; alert dedupe in `src/daemon/asks.rs` | S | a247.1 ~20 flips 22:51–22:54; "waiting for you" ×13, "Update" ×11 (t12v); #841 ×5; 268 alerts / 216 distinct |
| 3 | Job news goes to the session's brain first ("ended", "may be stuck"). The user gets a card only when the brain raises it | `src/daemon/running.rs:708,712` | S | 81 job alerts; ran-0-min and stuck-on-purpose cases above |
| 4 | Link cards and answers to their task and agenda (`Decision.task` for every kind; `task` on relayed answers and history entries); keep closed cards a week | `src/decisions.rs:48,620`; `src/daemon/asks.rs`; `src/history.rs` | S–M | baseline had to join by time (G9) |
| 5 | The completion metric: record `helped` on agendas; `tower stats`; the panel line and the phone | `src/agenda.rs`, new `src/stats.rs`, `src/orb/panel/home.rs`, `space.rs`, `src/daemon/lan/quiet.rs` | M | G1; baseline 15/44 |
| 6 | Structured handoff out: dispatch fields rendered in a fixed template; launch, retry, requeue and failover use it | `src/dispatch.rs:131,358`; `src/launch.rs:699`; `src/daemon/agendas.rs:307`; `src/failover.rs:477`; tower-core.md:71 | M | G2: a259.1, a282.3 |
| 7 | Structured handoff back: the Handoff block, parsed into verify, checkpoint, card or failure; keyword judging only as a fallback | `src/daemon/asks.rs` `finished`; `agenda::judge`; `src/daemon/watch.rs:1306`; `introduce` | M–L | G3: 75 keyword verifies, a292.1; 10 claimed-then-reworked |
| 8 | An evaluator before "verified": a headless check of the Handoff and evidence against the instruction and done-when (as `covered` does), when there is no Evidence or the work is a merge or deploy | new beside `src/covered.rs`; `agenda::assess` | M | G3, G4 |
| 9 | An alternate path before the user: on a first failure, `next` asks for a different approach; on a second, the brain re-plans (another session, a fresh worktree) before a card | `src/daemon/agendas.rs:288-343` | M | G4 |
| 10 | Resume from the checkpoint: requeue, re-prompt and carry-on send the last Handoff's `so_far` and `next`, not the first instruction | `src/agenda.rs:1159`; `src/failover.rs:477` | S after 7 | G7 |
| 11 | Send retries with exponential backoff and a cap, then park with a card | `src/daemon/sendq.rs:294,1606` | S | G8 (no harm seen tonight) |

## Top five

1. Make `tower job done` name its task (#1).
2. Count one ask per question, not per flicker (#2).
3. Send job news to the session's brain first (#3).
4. Measure completion without help, and show it in the panel, after linking cards to tasks (#4 and #5).
5. Structured handoffs both ways, with the Handoff block replacing keyword verification (#6 and #7).

Today's baseline to beat: **15 of 44 requests (34%) on their own, 19 (43%) with only approvals, the user asked 66 times.**
