# Planning v2: plans for complex work

Today a plan (`tower plan`, `src/plans.rs`) is written once by the planner, published as an Artifact, talked over in a chat, and closed at launch. That falls short on complex tasks in three ways:

- open questions sit on the artifact unanswered;
- the plan never changes once the work starts;
- "done" is only said per PR (its checks), never per stage of the work.

Planning v2 makes the plan the spine of the work:

- staged, with what counts as done and how each stage is validated before and after;
- its questions answered through the tower;
- revised as the sessions learn things.

## Data model (`src/plans.rs`, kept in `plans.json`)

A `Plan` keeps what it has (draft, ticket, PRs, chat, artifact link) and gains these fields.

| Field | What |
|---|---|
| `stages: Vec<Stage>` | The milestones, in order. |
| `assumptions: Vec<Assumption>` | What the plan takes for granted, each with how and when it is checked. |
| `questions: Vec<Question>` | What the plan can't decide alone. |
| `findings: Vec<Finding>` | What sessions learned while working (slice 2). |
| `revisions: Vec<Revision>` | Every change to the plan: what, why, which finding caused it (slice 2). |
| `critic: Critic` | The critic pass on the draft: the gaps it found and when (slice 3). |

### `Stage`

- `id` (`s1`…), `title`.
- `kind`:
  - `spike` (an unknown, settled first);
  - `build`;
  - `release`.
- `goal`, `done_when`.
- `after`: the ids of the stages it waits for.
- `prs`: the plan's PR numbers it delivers.
- `pre: Vec<Check>`: checked *before* work starts (assumptions, access, data, environments).
- `post: Vec<Check>`: checked *after* (the exact tests, commands and live checks, and the evidence each gives).

A `Check` is the PR check that already exists (`prscore::Check`: `id`, `what`, `how`, `evidence`, `status`, `proof`, `by`, `at_ms`). Reporting one is the same act as `tower validate`.

A stage's state is computed, never set by hand:

| State | When |
|---|---|
| `waiting` | A stage in its `after` isn't done. |
| `ready` | Its pre-checks are not all passed. |
| `active` | Pre passed; work goes on. |
| `failing` | A check failed or wasn't run. |
| `done` | Every pre and post check passed with evidence, and every one of its PRs that has opened has all its planned checks passed (`prs::Validation::all_passed`). |

### `Assumption`

- `id` (`a1`…), `text`.
- `check`: how it is checked.
- `by`: the stage it must hold by.
- `status`:
  - `open`;
  - `holds`;
  - `broken`, with `proof`.

A broken assumption makes a finding of its own, so the plan is revised.

### `Question`

- `id` (`q1`…), `text`, `why`.
- `options`: each with a label and why; `recommended`: the index of the planner's pick.
- `theirs`: the planner says only the user can answer it.
- `stage`: the stage it blocks, if any.
- `status`:
  - `open` (just written);
  - `with-tower` (the brain weighs it);
  - `asked` (a card in the user's queue);
  - `answered`.
- `answer`, `by` (`you` or `tower`), `card` (the decision's id), `at_ms`.

### `Finding` (slice 2)

- `id` (`f1`…), `at_ms`, `session`, `task`, `stage`.
- `kind`:
  - `assumption-wrong`;
  - `split-pr`;
  - `risk`;
  - `check-cant-run`;
  - `scope`;
  - `other`.
- `text`, `assumption`.
- `revision`: the revision that took it in.

### `Revision` (slice 2)

- `n`, `at_ms`.
- `by`: `planner`, `tower` or `you`.
- `why`, `finding`.
- `changes`: one line per change.
- `size`:
  - `small` (wording, a check added, a PR split inside a stage, a new risk noted): applied by itself and noted;
  - `big` (scope, a stage's done-when, the goal, a risk the user must accept, a stage removed): one decision for the user, with the diff.
- `status`: `applied`, `proposed`, `accepted` or `rejected`.
- `decision`.

## The flow

```
draft ──► critic ──► questions ──► approve (launch) ──► execute ──► revise ──► done
  ▲                     │                                  │           │
  └─────── planner revises (answers, critic gaps) ─────────┘◄──────────┘ findings
```

1. **Draft.** The planner writes `plan.md` and the PRs (`tower plan prs`), as today. It also writes the structure, `tower plan stages <id> --json-file stages.json`, with:
   - the stages: spikes first for what is unknown or risky;
   - the assumptions;
   - the questions.

   The tower checks it before taking it:
   - every stage has a done-when and at least one post-check;
   - a stage's PRs exist in the plan;
   - `after` names real stages, with no cycles;
   - every question has an id and text.

   Then the planner publishes.
2. **Critic** (slice 3). Once the plan is published, a separate one-shot reviewer reads `plan.md` and the structure for:
   - gaps;
   - a stage without real validation;
   - assumptions no one checks;
   - questions it should ask but doesn't.

   Its gaps go to the planner as one note, and it revises. Only then do questions go out, so the user sees the plan once, with only the real questions.
3. **Questions.** Each open question is routed:
   - **To the user** when the planner marked it `theirs`, or when the tower's escalation rules say so: the same words as a session's question (`questions::must_escalate`: secrets, deploys, deletes, prod data, money). It becomes a card in the user's queue (`decisions::Kind::Choice`, key `plan:<plan>:<q>`, why `plan`), with the planner's options and its pick. It shows on the Mac and the phone like any card.
   - **Otherwise to the brain**, as a note: `[plan-question] <plan> <q> {…}` with the question, the options and the pick. Under the 90/10 rule the brain answers what is routine (`tower plan answer <plan> <q> "<answer>" --by tower`) and hands on what isn't (`tower plan ask <plan> <q>`). No word in 2 minutes: it becomes a card.
   - **An answer from anywhere** (the card on Mac or phone, the brain, `tower plan answer`) is written into the plan: answer, by whom, when. Its card closes, and the planner is told to put it in `plan.md` and republish the artifact.
   - **The page lists each question with its answer.** None sits open there: the artifact shows an open one as "asked in the tower".
   - **A launch with open questions is refused** (`tower launch --plan`) unless forced, and the Launch button says why.
4. **Approve.** The user launches with the plan, as today. The plan no longer closes. It becomes `live`: kept, followed and revised, with the space it launched.
5. **Execute** (slice 2). Every task sent from a live plan carries the structured handoff from the gap review (`docs/orchestration-gap-review-2026-11-14.md`): goal, stage, done-when, so far, constraints, uncertain, next. It closes with "end your turn with a Handoff block". Tasks, agenda tasks and PRs carry their stage (`stage: s2`). The spike stage's tasks go first; the build stages wait for its findings.
6. **Revise** (slice 2).
   - **Findings come in two ways:** sessions report them (`tower plan finding <plan> --kind … "<text>" [--stage s2] [--assumption a1]`), and the Handoff block's `Uncertain` and `Left` lines.
   - **The planner turns a finding into a revision** (`tower plan revise <plan> --json-file stages.json --why "…" --finding f2`). The tower sizes the change by comparing the structure before and after:
     - **small:** applied, noted in the revision log and on the page;
     - **big:** one card with the diff (the stages, done-whens and risks before and after), applied only on the user's yes.
   - **Every revision bumps the plan's version** and republishes the artifact with a "What changed" section.
7. **Done.** A stage is done only when its checks passed with evidence. The plan is done when every stage is, and the space's goal follows.

## Commands

| Command | Who | Slice |
|---|---|---|
| `tower plan stages <id> --json-file <f>` | planner | 1 |
| `tower plan answer <id> <q> "<answer>" [--by tower\|you]` | brain, user | 1 |
| `tower plan ask <id> <q>` | brain | 1 |
| `tower plan check <id> <stage> <check> passed\|failed\|not-run --evidence "…"` | sessions, brain | 1 |
| `tower plan assume <id> <a> holds\|broken --evidence "…"` | sessions | 2 |
| `tower plan finding <id> --kind <k> "<text>" [--stage s] [--assumption a]` | sessions | 2 |
| `tower plan revise <id> --json-file <f> --why "…" [--finding f]` | planner | 2 |

## The UI

**Mac** (the New space flow's plan section, `src/orb/panel/launch.rs`; the space view once live):

- **Stages,** "Stage 2 of 4", one card each:
  - its title and kind;
  - done-when;
  - pre and post checks, each with its status and evidence;
  - its PRs and their check line;
  - a state chip (waiting, ready, active, failing, done).
- **Questions:**
  - open ones say where they are ("with the tower", "in your queue");
  - answered ones show the answer and who gave it.
  The cards themselves are in the queue, like any.
- **Assumptions,** each with its check and its status.
- **Revisions** (slice 2): the version, and what changed and why, newest first. A big one pending shows its card.
- **The launch button** says why it waits ("2 questions open").

**Phone** (`/v1/state` → `plan`, the Missions screen's plan row and the plan screen):

- the same `stages`, `questions`, `assumptions` and `revisions` fields;
- the plan's question cards come through `/v1/decisions` like any choice;
- the Android session (t157) gets the exact fields once the API serves them.

## Slices

1. Stages with done-when and pre/post checks in the plan and its artifact; open questions routed to the brain or the user's queue; answers written back; stage checks; the launch gate; the Mac view.
2. The living plan: live after launch; tasks linked to stages with the structured handoff; findings; revisions (small applied, big as one card with the diff); assumptions checked; stage progress on the panel and the phone.
3. The critic pass before questions go out; spikes first.
