#!/usr/bin/env python3
"""Made-up tower state for the README's screenshots: no real sessions,
projects, people or accounts.

    python3 docs/screenshots/demo_state.py <dir>

writes <dir>/ui/{state,spaces,prs}.json, <dir>/plan.json and <dir>/draft.json.
Then, with a sandbox home so nothing real is read:

    HERDER_TOWER_HOME=<dir>/home HERDER_TOWER_STATE=<dir>/state HERDER_TOWER_DATA=<dir>/data \
    HERDER_TOWER_ORB_GHOST=1 HERDER_TOWER_ORB_WINDOW='show:space=checkout-redesign' \
    tower orb --ui-dir <dir>/ui
"""
import json
import os
import sys
import time

out = sys.argv[1] if len(sys.argv) > 1 else "demo"
now = int(time.time() * 1000)
MIN, HOUR, DAY = 60_000, 3_600_000, 86_400_000


def ago(ms):
    return now - ms


# ---- sessions ----

def progress(line, turn, last, **kw):
    return {"line": line, "turn_ms": ago(turn), "last_ms": ago(last), "done": kw.pop("done", 0), **kw}


storefront_items = [
    {"id": "1", "subject": "Read the checkout flow", "active": "Reading the checkout flow", "status": "completed"},
    {"id": "2", "subject": "Add saved cards to the payment step", "active": "Adding saved cards", "status": "completed"},
    {"id": "3", "subject": "Run the checkout test suite", "active": "Running the checkout test suite", "status": "in_progress"},
    {"id": "4", "subject": "Open the PR into dev", "active": "Opening the PR", "status": "pending"},
    {"id": "5", "subject": "Check it on the preview deploy", "active": "Checking the preview", "status": "pending"},
]
sessions = {
    "t32": dict(name="storefront", status="working", account="work",
                said="Saved cards are in the payment step; running the checkout suite now.",
                progress=progress("step 3 of 5: running the checkout test suite · Bash: npm test -- checkout for 3 min · 41 min in · last update just now",
                                  41 * MIN, 20_000, step=3, steps=5, done=2, items=storefront_items,
                                  item="Running the checkout test suite", tool="Bash: npm test -- checkout", tool_ms=ago(3 * MIN)),
                brief={"text": "Adding saved cards to checkout: the payment step is done, the test suite runs now.",
                       "asked": "Customers can pay with a saved card in one step",
                       "done": ["Saved cards in the payment step"], "now": "step 3 of 5: running the checkout test suite",
                       "next": ["Open the PR into dev", "Check it on the preview deploy"], "changed_ms": ago(2 * MIN), "history": []}),
    "t33": dict(name="api", status="working", account="work",
                said="The card vault endpoint is in; the migration waits for your yes.",
                progress=progress("Bash: Run the integration tests for 4 min · 1 h 5 min in · last update 1 min ago",
                                  65 * MIN, 70_000, tool="Bash: Run the integration tests", tool_ms=ago(4 * MIN)),
                brief={"text": "The card vault API is built; its migration waits for you.", "asked": "A card vault endpoint the checkout can use",
                       "done": ["POST /cards and GET /cards", "Tokens stored with the provider, never the card"], "now": "running the integration tests",
                       "next": ["Run the migration on dev"], "changed_ms": ago(4 * MIN), "history": []}),
    "t34": dict(name="review storefront 214", status="done", account="personal",
                said="Agreed: no critical issues in #214."),
    "t41": dict(name="ranker", status="done", account="personal",
                said="Training runs in the background; I'll report the eval when it ends.",
                job={"what": "Train the 1.2B ranker on v2 data", "since_ms": ago(2 * HOUR + 10 * MIN), "progress_ms": ago(40_000), "stuck": False, "more": 0,
                     "line": "running a job: Train the 1.2B ranker on v2 data, since %s, last progress just now" % time.strftime("%H:%M", time.localtime((now - 2 * HOUR - 10 * MIN) / 1000))},
                brief={"text": "Training the ranker; the eval follows.", "asked": "Search answers in under 2 s", "done": ["v2 training data labelled"],
                       "now": "Train the 1.2B ranker on v2 data (a job, 2 h in)", "next": ["Evaluate v1 and v2 on real queries"], "changed_ms": ago(40 * MIN), "history": []}),
    "t42": dict(name="search api", status="working", account="personal",
                progress=progress("step 2 of 4: wiring the ranker behind a flag · 18 min in · last update just now", 18 * MIN, 15_000,
                                  step=2, steps=4, done=1, item="Wiring the ranker behind a flag"),
                said="The flag is in; wiring the ranker behind it."),
    "t51": dict(name="docs", status="done", account="work", said="The new pages are live on the preview."),
}


def member(addr):
    s = sessions[addr]
    return {"address": addr, "name": s["name"], "status": s["status"], "account": s["account"], "said": s.get("said"),
            "progress": s.get("progress"), "brief": s.get("brief"), "job": s.get("job")}


def active(now_on, stretch, today, total, waiting=False):
    return {"at_ms": now, "now": now_on, "since_ms": ago(stretch), "today_ms": today, "total_ms": total,
            "waiting_now": waiting, "waiting_today_ms": 12 * MIN if waiting else 0, "waiting_total_ms": 40 * MIN}


missions = [
    {
        "name": "checkout-redesign", "goal": "Customers can pay with a saved card in one step",
        "done_when": "A saved card pays on the preview deploy, with tests", "goal_history": [], "standing": False, "chat": False,
        "review": True, "state": "open", "follow": True, "mute": False, "needs": 2,
        "report": "", "sessions": [member("t32"), member("t33"), member("t34")],
        "requests": [{"id": "a12", "title": "Saved cards in checkout", "state": "open", "done": 2, "total": 5}],
        "thread_count": 6, "tower": {"name": "tower-checkout-redesign", "status": "done", "address": "t31", "account": "work"},
        "quiet_since_ms": None, "ready_since_ms": None, "auto_close_ms": None,
        "active": active(True, 52 * MIN, 3 * HOUR + 10 * MIN, 9 * HOUR + 40 * MIN, waiting=True),
        "brief": {"text": "Saved cards in checkout: the API and the payment step are built, tests run now.",
                  "goal": "Customers can pay with a saved card in one step", "stage": "Build: stage 2 of 3",
                  "done": ["Card vault API (api)", "Saved cards in the payment step (storefront)", "Review of #214 agreed"],
                  "doing": ["storefront: step 3 of 5: running the checkout test suite", "api: running the integration tests"],
                  "next": ["Open the PR into dev", "Check it on the preview deploy"],
                  "waits": ["The migration on dev waits for your yes", "#214 waits for your merge"],
                  "risks": ["The migration adds a table: it ships with the next release to prod"],
                  "changed_ms": ago(2 * MIN), "history": [{"at_ms": ago(30 * MIN), "text": "Building the card vault API."}]},
    },
    {
        "name": "search-speedup", "goal": "Search answers in under 2 s with a fine-tuned ranker",
        "done_when": "p95 under 2 s on real queries, recall no worse", "goal_history": [], "standing": False, "chat": False,
        "review": False, "state": "open", "follow": True, "mute": False, "needs": 0, "report": "",
        "sessions": [member("t41"), member("t42")], "requests": [{"id": "a9", "title": "Fine-tune the ranker", "state": "open", "done": 3, "total": 6}],
        "thread_count": 3, "tower": {"name": "tower-search-speedup", "status": "done", "address": "t40", "account": "personal"},
        "quiet_since_ms": None, "ready_since_ms": None, "auto_close_ms": None,
        "active": active(True, 2 * HOUR + 10 * MIN, 5 * HOUR, 14 * HOUR),
        "brief": {"text": "The ranker trains; the search API gets it behind a flag.", "goal": "Search answers in under 2 s with a fine-tuned ranker",
                  "stage": "Train and wire", "done": ["v2 training data labelled"], "doing": ["ranker: training (2 h in)", "search api: wiring the ranker"],
                  "next": ["Evaluate v1 and v2 on real queries"], "waits": [], "risks": [], "changed_ms": ago(10 * MIN), "history": []},
    },
    {
        "name": "docs-site", "goal": "The docs site has the new API pages", "done_when": "", "goal_history": [], "standing": False, "chat": False,
        "review": False, "state": "ready", "follow": False, "mute": False, "needs": 0, "report": "The new API pages are live on the preview.",
        "sessions": [member("t51")], "requests": [], "thread_count": 0,
        "tower": {"name": "", "status": "off", "address": "", "account": ""},
        "quiet_since_ms": ago(25 * MIN), "ready_since_ms": ago(25 * MIN), "auto_close_ms": now + 35 * MIN,
        "active": active(False, 0, 40 * MIN, 2 * HOUR),
        "brief": {"text": "Done: the API pages are live on the preview.", "goal": "The docs site has the new API pages", "stage": "Ready to close",
                  "done": ["API pages written", "Preview deployed"], "doing": [], "next": [], "waits": [], "risks": [], "changed_ms": ago(25 * MIN), "history": []},
    },
]

# ---- decisions ----

pr214 = {
    "key": "acme/storefront#214", "repo": "acme/storefront", "number": 214, "base": "dev",
    "title": "Saved cards in one-step checkout", "url": "https://github.com/acme/storefront/pull/214",
    "checks": "passing", "note": "", "session": "t32",
    "merge": {"can": True, "blocked": None, "needs_override": None, "warnings": []},
    "review": {"status": "agreed", "rounds": 1, "critical": 0, "reviewer": "t34", "reviewer_name": "review storefront 214", "line": "Review agreed: nothing critical"},
    "score": {"needs_review": False, "line": "Low risk: UI and tests, as the plan predicted", "reasons": [], "flags": [], "planned": "p2#1"},
    "validation": {"checks": [
        {"id": "c1", "what": "A saved card pays", "how": "npm test -- checkout", "evidence": "test output", "status": "passed", "proof": "34 passed"},
        {"id": "c2", "what": "The preview takes a saved card", "how": "pay on the preview with the test card", "evidence": "receipt", "status": "planned"}],
        "edge_cases": [{"case": "An expired saved card", "checks": ["c1"]}]},
    "impact": {"dependents": [], "outside": [], "line": "Nothing outside depends on what it changes"},
    "summary": "Adds a saved-card picker to the payment step, behind the card vault API.",
}
decisions = [
    {"id": "d1", "key": "merge::acme/storefront#214", "kind": "merge", "space": "checkout-redesign",
     "sessions": [{"address": "t32", "name": "storefront", "waiting": True}], "waiting": 1,
     "title": "Merge #214 into dev: Saved cards in one-step checkout",
     "detail": "Checks pass, the review agreed, low risk.", "why": "merge", "options": [], "prs": [pr214],
     "first_ms": ago(6 * MIN), "last_ms": ago(1 * MIN), "count": 1, "state": "open"},
    {"id": "d2", "key": "permission::t33", "kind": "permission", "space": "checkout-redesign",
     "sessions": [{"address": "t33", "name": "api", "waiting": True}], "waiting": 1,
     "title": "api wants to run the card vault migration on dev",
     "detail": "npm run db:migrate -- --env dev", "why": "data",
     "options": [{"label": "Yes", "why": "dev only; it adds the cards table", "pick": True}, {"label": "No", "why": "", "pick": False}],
     "first_ms": ago(12 * MIN), "last_ms": ago(2 * MIN), "count": 1, "state": "open"},
    {"id": "d3", "key": "question::t42", "kind": "question", "space": "search-speedup",
     "sessions": [{"address": "t42", "name": "search api", "waiting": True}], "waiting": 1,
     "title": "Ship the new ranker to all users at once, or to 10% first?",
     "detail": "The flag is in. The eval of v2 isn't done yet.", "why": "design",
     "options": [{"label": "10% first", "why": "the eval isn't in; a slow ranker would show on a few", "pick": True},
                 {"label": "Everyone", "why": "faster to learn, riskier", "pick": False}],
     "first_ms": ago(3 * MIN), "last_ms": ago(3 * MIN), "count": 1, "state": "open"},
]

threads = {"checkout-redesign": [
    {"at": ago(48 * MIN), "from": "t33", "from_name": "api", "to": "t32", "to_name": "storefront",
     "text": "POST /cards is on dev: send the provider token, never the card.", "space": "checkout-redesign", "delivered": True},
    {"at": ago(44 * MIN), "from": "t32", "from_name": "storefront", "to": "t33", "to_name": "api",
     "text": "Thanks. Does GET /cards return the brand and the last four?", "space": "checkout-redesign", "delivered": True},
    {"at": ago(43 * MIN), "from": "t33", "from_name": "api", "to": "t32", "to_name": "storefront",
     "text": "Yes: brand, last4, exp_month, exp_year.", "space": "checkout-redesign", "delivered": True},
]}

releases = {"checkout-redesign": {"space": "checkout-redesign", "checked_ms": ago(3 * MIN), "summary": "1 change on dev, not on prod yet",
    "repos": [{"repo": "acme/api", "dev": "dev", "prod": "main",
               "changes": [{"number": 88, "title": "Card vault endpoint", "url": "https://github.com/acme/api/pull/88", "base": "dev", "stage": "dev",
                            "merged_ms": ago(70 * MIN), "released_ms": 0, "release": 0, "skipped_dev": False, "stage_line": "on dev since %s" % time.strftime("%H:%M", time.localtime((now - 70 * MIN) / 1000))}],
               "releases": [], "lines": [], "on_prod": [], "on_dev_not_prod": [88]}]}}

agents = [{"address": a, "name": s["name"], "status": s["status"], "app": "claude-code", "account": s["account"], "question": None,
           "folder": "/w/" + s["name"].split()[0], "queued": 0, "progress": s.get("progress"),
           "jobs": [{"what": s["job"]["what"], "since_ms": s["job"]["since_ms"], "progress_ms": s["job"]["progress_ms"], "busy": True, "stuck": False,
                     "pid": 4242, "source": "background", "command": "python train.py --model 1.2b"}] if s.get("job") else []}
          for a, s in sessions.items()]

state = {
    "version": 3, "updated_ms": now, "pid": 1, "running": True, "mode": "ptt", "mic": "ready", "armed": False, "transcribing": False,
    "speaking": False, "muted": False, "tell_voice": "decisions", "voice_on_mac": True, "announcing": False, "brain": "tower", "brain_app": "claude-code",
    "hearing": "", "heard": "", "heard_ms": 0, "dictated": "", "dictated_ms": 0, "said": "", "said_ms": 0, "alerts": [], "missed_while_muted": [],
    "agents": agents, "error": "", "talk_key": "ctrl", "voice": "replies", "stt_model": "large-v3-turbo-q5", "lan": "off", "lan_hosts": [],
    "screenshots": [], "questions": [], "question_policy": "normal", "tell_level": "decisions", "auto_merge": False,
    "threads": threads, "decisions": decisions, "decisions_count": len(decisions), "decisions_held": [],
    "missions": {"spaces": missions, "quiet": [], "closed_today": ["release-notes", "flaky-login-test"]},
    "brains": [{"name": "tower", "space": None, "address": "t10", "status": "done", "model": "claude-opus-5-5", "effort": "high", "account": "work"},
               {"name": "tower-checkout-redesign", "space": "checkout-redesign", "address": "t31", "status": "done", "model": "claude-opus-5-5", "effort": "high", "account": "work"}],
    "brain_model": {"program": "claude", "model": "claude-opus-5-5", "effort": "high",
                    "choices": {"models": [{"id": "claude-opus-5-5", "label": "Opus 5.5"}, {"id": "claude-sonnet-5-5", "label": "Sonnet 5.5"}], "efforts": ["low", "medium", "high"]},
                    "running": [], "restart_needed": False, "locked_by_args": False, "says": "The brains run Opus 5.5 at high effort."},
    "releases": releases, "acts": [], "plan": None, "plans_live": [], "reviews": [],
    "accounts": {"list": [
        {"name": "work", "email": "work@example.com", "default": True, "five_hour": None, "seven_day": {"used": 62.0, "resets_ms": now + 2 * DAY + 5 * HOUR}, "out_until_ms": None, "out_why": ""},
        {"name": "personal", "email": "me@example.com", "default": False, "five_hour": None, "seven_day": {"used": 24.0, "resets_ms": now + 4 * DAY}, "out_until_ms": None, "out_why": ""}],
        "starts_on": "work", "moves": [{"at_ms": ago(3 * HOUR), "address": "t41", "name": "ranker", "space": "search-speedup", "tower": "", "from": "work", "to": "personal", "session_id": "s-demo", "how": "moved"}],
        "moving": [], "waiting": []},
    "phase": "ready",
}

ui = os.path.join(out, "ui")
os.makedirs(ui, exist_ok=True)
for d in ("home", "state", "data"):
    os.makedirs(os.path.join(out, d), exist_ok=True)
json.dump(state, open(os.path.join(ui, "state.json"), "w"), indent=1)
json.dump({"focus": None, "agendas": {}, "read_ms": {}, "skipped_ms": {}, "missions": {}}, open(os.path.join(ui, "spaces.json"), "w"))
pr_row = dict(pr214, head="checkout-redesign/storefront", head_sha="3f2c1ab", state="open", draft=False, mergeable="mergeable", merge_state="clean",
              checks={"state": "passing", "failing": [], "pending": [], "passed": 6, "total": 6}, name="storefront", space="checkout-redesign",
              waiting=True, found_ms=ago(30 * MIN), checked_ms=ago(1 * MIN), closed_ms=0, merged_ms=0, merge_commit="", merged_by="", settled=False,
              body="Adds a saved-card picker to the payment step.", described={})
json.dump({"prs": [pr_row]}, open(os.path.join(ui, "prs.json"), "w"), indent=1)

# ---- a plan (the launcher's plan view) ----
check = lambda i, what, how, status="planned", proof="": {"id": i, "what": what, "how": how, "evidence": "output", "status": status, "proof": proof}
plan = {
    "id": "p2", "form": "page", "status": "ready", "ticket": "Customers can pay with a saved card in one step", "draft": {}, "revision": 2,
    "planner": "tower-planner", "url": "https://claude.ai/artifact/demo", "why": "", "created_ms": ago(3 * HOUR), "ready_ms": ago(150 * MIN),
    "version": 2, "launched": "checkout-redesign", "open_questions": 1,
    "critic": {"status": "done", "at_ms": ago(160 * MIN), "gaps": ["No check that an expired card is refused"], "why": "Added as an edge case of stage 2."},
    "spikes_pending": [],
    "stages": [
        {"id": "s1", "title": "Provider spike", "kind": "spike", "done_when": "We know the provider stores cards and how long a token lives",
         "post": [check("c1", "A test card is stored", "node spikes/vault.js", "passed", "token tok_test, 90 days")], "state": "done"},
        {"id": "s2", "title": "Card vault API", "kind": "build", "goal": "The API stores and lists saved cards", "done_when": "POST and GET /cards answer on dev",
         "after": ["s1"], "prs": [1], "pre": [check("c0", "The migration runs on a copy", "npm run db:migrate -- --dry-run", "passed", "1 table")],
         "post": [check("c2", "POST /cards stores a token", "npm test -- cards", "passed", "18 passed")], "state": "done"},
        {"id": "s3", "title": "Saved cards in checkout", "kind": "build", "goal": "A saved card pays in one step", "done_when": "It pays on the preview deploy",
         "after": ["s2"], "prs": [2], "post": [check("c3", "A saved card pays", "npm test -- checkout", "running"), check("c4", "It pays on the preview", "pay with the test card")],
         "state": "building", "tasks": [{"id": "t1", "title": "Saved-card picker", "state": "verified", "session": "t32"}, {"id": "t2", "title": "Checkout tests", "state": "working", "session": "t32"}]},
    ],
    "assumptions": [{"id": "a1", "text": "The provider keeps a card token 90 days", "check": "the spike's stored card", "by": "s1", "status": "holds", "proof": "token lives 90 days"},
                    {"id": "a2", "text": "No customer has more than 10 saved cards", "check": "a count on prod data", "by": "s3", "status": "open"}],
    "questions": [{"id": "q1", "text": "Keep saved cards for 2 years?", "status": "answered", "answer": "Yes, 2 years", "by": "tower"},
                  {"id": "q2", "text": "Show the card brand's logo?", "status": "asked", "theirs": True, "card": "d4"}],
    "findings": [{"id": "f1", "at_ms": ago(70 * MIN), "kind": "assumption-wrong", "text": "The provider token lives 90 days, not a year", "stage": "s1", "session": "t33", "revision": 2}],
    "revisions": [{"n": 2, "at_ms": ago(65 * MIN), "by": "planner", "why": "The token lives 90 days: refresh it on use", "findings": ["f1"],
                   "changes": ["Stage 2 refreshes a token older than 60 days"], "size": "small", "status": "applied"}],
    "progress": {"stages": 3, "done": 2, "at": 3},
    "prs": [
        {"n": 1, "repo": "acme/api", "base": "dev", "title": "Card vault endpoint", "changes": "POST/GET /cards", "files": ["src/cards.ts", "db/migrations/0042_cards.sql"],
         "migration": True, "breaking": "", "escalate": [], "after": [], "checks": [], "edge_cases": [],
         "score": {"needs_review": True, "line": "Needs review: a database migration (db/migrations/0042_cards.sql)", "reasons": ["a database migration"], "flags": ["migration"]}, "pr": "acme/api#88"},
        {"n": 2, "repo": "acme/storefront", "base": "dev", "title": "Saved cards in one-step checkout", "changes": "the payment step", "files": ["src/checkout/payment.tsx"],
         "migration": False, "breaking": "", "escalate": [], "after": [1], "checks": [], "edge_cases": [],
         "score": {"needs_review": False, "line": "Low risk: UI and tests", "reasons": [], "flags": []}, "pr": "acme/storefront#214"},
    ],
    "thread": [{"from": "user", "text": "Keep cards for two years.", "at": ago(150 * MIN)}, {"from": "planner", "text": "Done: stage 2 deletes cards unused for two years.", "at": ago(149 * MIN)}],
}
json.dump(plan, open(os.path.join(out, "plan.json"), "w"), indent=1)
draft = {"space": "checkout-redesign", "goal": "Customers can pay with a saved card in one step", "done_when": "A saved card pays on the preview deploy, with tests",
         "project": "storefront", "notes": "", "assumed": {"account": "work"},
         "sessions": [{"repo": "storefront", "why": "the payment step", "account": "work"}, {"repo": "api", "why": "the card vault endpoint", "account": "work"}]}
json.dump(draft, open(os.path.join(out, "draft.json"), "w"), indent=1)
print("demo state in", out)
