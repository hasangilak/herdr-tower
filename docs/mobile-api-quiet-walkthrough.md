# The Quiet Tower's phone API, walked through

Every call of "The Quiet Tower" in [mobile-api.md](mobile-api.md), made with
`curl` against a running tower, in the order an app would make them: read
the queue, launch a space, set its goal, follow and mute it, answer a
decision, read a thread, ask a space's own tower, change the tell level
and the model, close the space, and what came over the event stream
meanwhile. The answers are what the tower sent; nothing here is made up.
The contract is mobile-api.md: this is its proof, and a place to copy a
shape from.

## How it was run

A tower built from this repository, in a sandbox of its own: its own
config, state and data folders, no voice (`HERDER_TOWER_VOICE=off`), and
stand-ins for Herdr (`hc`, `herdr-app`), the Claude accounts (`cca`), `gh`
and the brain's program. Its phone API listened on `127.0.0.1:17943` only
(`[lan] bind = "127.0.0.1"`, `tailscale = "off"`). So:

- the sessions are stand-ins: they open, take what they are sent and stop
  when the walk tells them to, and never write anything themselves (their
  `account` and `said` are `null` here; on a real tower they are filled);
- the towers' brains are stand-ins too and never answer: where an answer is
  needed to show the chat, the walk writes it into the history as a
  brain's hook would, and says so;
- paths are shown from `/sandbox`; long answers are cut where it says so
  (`… n more`, "left out here"), and nowhere else.

The phone is `curl`, trusting the tower's own certificate and no other
(the certificate's name is `herder-tower`, so `curl` is told that this name
is 127.0.0.1; the app pins the fingerprint instead and checks no name):

```
on the Mac$ tower phone pair --json
{"code": "72739959", "host": "127.0.0.1", "port": 17943, "hosts": [{"kind": "lan", "host": "127.0.0.1", "port": 17943}],
 "fingerprint": "cd9f5c450d5b86162a5878e318b4adc6d6e9b9a837b2aed67a024c363ded4648", "expires_in_s": 300, …}

$ openssl x509 -inform der -in <the tower's lan/cert.der> -out cert.pem
$ shasum -a 256 <the tower's lan/cert.der>        # the fingerprint the QR carries
cd9f5c450d5b86162a5878e318b4adc6d6e9b9a837b2aed67a024c363ded4648

$ curl --cacert cert.pem --resolve herder-tower:17943:127.0.0.1 \
       -X POST https://herder-tower:17943/v1/pair -d '{"code": "72739959", "name": "curl phone"}'
→ 200 {"token": "htk_…", "device": {"id": "d_7463a95d", "name": "curl phone"}, "fingerprint": "cd9f5c450d5b8616…", "api": 1, "features": ["quiet"]}
```

Every call below carries `Authorization: Bearer <that token>` unless it
says otherwise. `→` is the status and the body that came back.

## 1. Who is there

```
GET /v1/hello
→ 200 {
  "api": 1,
  "tower": "0.1.0",
  "device": {"id": "d_7463a95d", "name": "curl phone"},
  "voice": "af_bella",
  "phone_is_voice": false,
  "audio_route": "phone",
  "features": ["quiet"]
}
```
```
GET /v1/decisions        (no Authorization header)
→ 401 {"error": {"code": "unauthorized", "message": "missing bearer token"}}
```

## 2. A space from one sentence

The setups a launch can use, and what a launch would do (nothing is opened):

```
GET /v1/setups
→ 200 {
  "setups": [
    {
      "name": "eyzflow + mediaroom on orca",
      "account": "orca",
      "repos": ["/sandbox/work/eyz/eyzflow", "/sandbox/work/eyz/mediaroom"]
    },
    {"name": "notes on default", "account": "default", "repos": ["/sandbox/work/notes"]}
  ]
}
```
```
POST /v1/launch
{"goal": "Fix the login redirect", "dry_run": true}
→ 200 {
  "space": "fix-login",
  "goal": "Fix the login redirect",
  "made": true,
  "dry_run": true,
  "agenda": null,
  "sessions": [
    {
      "name": "fix-login-eyzflow",
      "repo": "/sandbox/work/eyz/eyzflow",
      "address": null,
      "account": "orca",
      "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/eyzflow",
      "branch": "fix-login/eyzflow",
      "port": null,
      "task": null,
      "error": null
    },
    {
      "name": "fix-login-mediaroom",
      "repo": "/sandbox/work/eyz/mediaroom",
      "address": null,
      "account": "orca",
      "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/mediaroom",
      "branch": "fix-login/mediaroom",
      "port": null,
      "task": null,
      "error": null
    }
  ],
  "assumed": {
    "setup": "eyzflow + mediaroom on orca",
    "setup_why": "the first of 2",
    "account": "orca",
    "account_why": "the setup's",
    "name": "fix-login",
    "name_why": "the goal's first two words, small ones left out",
    "worktrees": true,
    "worktrees_why": "one per repo, on a branch of its own"
  }
}
```

The event stream is opened now and kept open for the rest of the walk (section 10 shows what came over it):

```
GET /v1/events        (curl -N, in the background)
```

For real. The answer comes at once, with the plan and an action's id; the sessions open after it:

```
POST /v1/launch
{"goal": "Fix the login redirect"}
→ 202 {
  "action": {
    "id": "act_3cbc0b4e",
    "kind": "launch",
    "space": "fix-login",
    "state": "running",
    "plan": "{… the plan, as the dry run above printed it …}",
    "receipt": null,
    "error": null,
    "by": "phone:d_7463a95d",
    "started_ms": 1794369724632,
    "finished_ms": null
  }
}
```

That call took 0.22 s. A second launch while it runs is refused, naming the one under way:

```
POST /v1/launch
{"goal": "Ship checkout v2"}
→ 409 {
  "error": {
    "code": "launching",
    "message": "\"fix-login\" is being launched: one at a time",
    "action": "act_3cbc0b4e"
  }
}
```
```
GET /v1/actions/act_3cbc0b4e
→ 200 {
  "action": {
    "id": "act_3cbc0b4e",
    "kind": "launch",
    "space": "fix-login",
    "state": "running",
    "plan": "{… the plan, as the dry run above printed it …}",
    "receipt": null,
    "error": null,
    "by": "phone:d_7463a95d",
    "started_ms": 1794369724632,
    "finished_ms": null
  }
}
  (asked right away: still opening)
```

Asked again until it is over (1.5 s after the launch was sent):

```
GET /v1/actions/act_3cbc0b4e
→ 200 {
  "action": {
    "id": "act_3cbc0b4e",
    "kind": "launch",
    "space": "fix-login",
    "state": "done",
    "plan": "{… the plan, as the dry run above printed it …}",
    "receipt": {
      "space": "fix-login",
      "goal": "Fix the login redirect",
      "made": true,
      "dry_run": false,
      "agenda": "a1",
      "sessions": [
        {
          "name": "fix-login-eyzflow",
          "repo": "/sandbox/work/eyz/eyzflow",
          "address": "t3",
          "account": "orca",
          "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/eyzflow",
          "branch": "fix-login/eyzflow",
          "port": 3001,
          "task": "a1.1",
          "error": null
        },
        {
          "name": "fix-login-mediaroom",
          "repo": "/sandbox/work/eyz/mediaroom",
          "address": "t4",
          "account": "orca",
          "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/mediaroom",
          "branch": "fix-login/mediaroom",
          "port": 3002,
          "task": "a1.2",
          "error": null
        }
      ],
      "assumed": {
        "setup": "eyzflow + mediaroom on orca",
        "setup_why": "the first of 2",
        "account": "orca",
        "account_why": "the setup's",
        "name": "fix-login",
        "name_why": "the goal's first two words, small ones left out",
        "worktrees": true,
        "worktrees_why": "one per repo, on a branch of its own"
      }
    },
    "error": null,
    "by": "phone:d_7463a95d",
    "started_ms": 1794369724632,
    "finished_ms": 1794369725680
  }
}
```

## 3. The spaces as missions

```
GET /v1/missions
→ 200 {
  "spaces": [
    {
      "name": "fix-login",
      "goal": "Fix the login redirect",
      "done_when": "",
      "standing": false,
      "state": "open",
      "follow": false,
      "mute": false,
      "needs": 0,
      "report": "",
      "sessions": [
        {
          "address": "t3",
          "name": "fix login eyzflow",
          "status": "working",
          "account": null,
          "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/eyzflow",
          "branch": "fix-login/eyzflow",
          "port": 3001,
          "said": null
        },
        {
          "address": "t4",
          "name": "fix login mediaroom",
          "status": "working",
          "account": null,
          "worktree": "/sandbox/work/eyz/.tower-worktrees/fix-login/mediaroom",
          "branch": "fix-login/mediaroom",
          "port": 3002,
          "said": null
        }
      ],
      "requests": [{"id": "a1", "title": "Fix the login redirect", "state": "open", "done": 0, "total": 2}],
      "thread_count": 0,
      "tower": {"name": "tower-fix-login", "status": "ready", "address": "t5"},
      "quiet_since_ms": null,
      "ready_since_ms": null,
      "auto_close_ms": null
    }
  ],
  "quiet": [],
  "closed_today": []
}
```

Its goal gets a done-when; the space is followed; muted and unmuted:

```
POST /v1/spaces/fix-login/goal
{"done_when": "The redirect lands on the dashboard, and both repos' tests pass"}
→ 200 {
  "space": "fix-login",
  "goal": "Fix the login redirect",
  "done_when": "The redirect lands on the dashboard, and both repos' tests pass",
  "standing": false,
  "state": "open",
  "report": ""
}
  (left out here: mission)
```
```
POST /v1/spaces/fix-login/follow
{"on": true}
→ 200 {"space": "fix-login", "follow": true}
  (left out here: mission)
```
```
POST /v1/spaces/fix-login/mute
{"on": true}
→ 200 {"space": "fix-login", "mute": true}
  (left out here: mission)
```
```
POST /v1/spaces/fix-login/mute
{"on": false}
→ 200 {"space": "fix-login", "mute": false}
  (left out here: mission)
```

A name is percent-decoded whole and matched in any case; a space nobody has is a 404:

```
POST /v1/spaces/FIX-LOGIN/follow
{"on": true}
→ 200 {"space": "fix-login", "follow": true}
  (left out here: mission)
```
```
POST /v1/spaces/No%20such%20space/goal
{"goal": "x"}
→ 404 {"error": {"code": "no_space", "message": "no space called \"No such space\""}}
```

## 4. A decision is raised, read, answered

Both sessions have stopped. The tower's brain raises a decision about the first, as it does when only the user can decide:

```
on the Mac$ tower raise t3 --why design "Tabs or a sidebar for the settings?" --options '[{"label":"Tabs",…},{"label":"A sidebar",…}]'
{
  "raised": true,
  "id": "d702e971f5cd1",
  "new": true,
  "folded": false,
  "kind": "choice",
  "title": "Tabs or a sidebar for the settings?",
  "count": 1,
  "waiting": 1,
  "session": "t3",
  "name": "fix login eyzflow",
  "space": "fix-login",
  "why": "design",
  "told": true
}
```
```
GET /v1/decisions
→ 200 {
  "space": null,
  "count": 1,
  "decisions": [
    {
      "id": "d702e971f5cd1",
      "key": "raise:fix-login:tabs or a sidebar for the settings",
      "kind": "choice",
      "space": "fix-login",
      "sessions": [{"address": "t3", "name": "fix login eyzflow", "waiting": true}],
      "waiting": 1,
      "title": "Tabs or a sidebar for the settings?",
      "why": "design",
      "options": [
        {"label": "Tabs", "why": "matches the mock", "pick": true},
        {"label": "A sidebar", "why": "more room for later sections", "pick": false}
      ],
      "pick": "Tabs",
      "prs": [],
      "question_id": null,
      "task": null,
      "first_ms": 1794369735747,
      "last_ms": 1794369735747,
      "count": 1,
      "state": "open",
      "muted": false
    }
  ]
}
```

The same thing raised again about the other session is the same decision (section 10 shows the two events):

```
on the Mac$ tower raise t4 --why design "tabs or a sidebar for the settings"
{
  "raised": true,
  "id": "d702e971f5cd1",
  "new": false,
  "folded": false,
  "kind": "choice",
  "title": "Tabs or a sidebar for the settings?",
  "count": 2,
  "waiting": 2,
  "session": "t4",
  "name": "fix login mediaroom",
  "space": "fix-login",
  "why": "design",
  "told": false
}
```
```
GET /v1/decisions/d702e971f5cd1
→ 200 {
  "decision": {
    "id": "d702e971f5cd1",
    "key": "raise:fix-login:tabs or a sidebar for the settings",
    "kind": "choice",
    "space": "fix-login",
    "sessions": [
      {"address": "t3", "name": "fix login eyzflow", "waiting": true},
      {"address": "t4", "name": "fix login mediaroom", "waiting": true}
    ],
    "waiting": 2,
    "title": "Tabs or a sidebar for the settings?",
    "why": "design",
    "options": [
      {"label": "Tabs", "why": "matches the mock", "pick": true},
      {"label": "A sidebar", "why": "more room for later sections", "pick": false}
    ],
    "pick": "Tabs",
    "prs": [],
    "question_id": null,
    "task": null,
    "first_ms": 1794369735747,
    "last_ms": 1794369737382,
    "count": 2,
    "state": "open",
    "muted": false
  }
}
```

While its space is muted it is out of the main queue and the count, and still in the space's own:

```
POST /v1/spaces/fix-login/mute
{"on": true}
→ 200 {"space": "fix-login", "mute": true}
  (left out here: mission)
```
```
GET /v1/decisions
→ 200 {"space": null, "count": 0, "decisions": []}
```
```
GET /v1/decisions?space=fix-login
→ 200 {
  "space": "fix-login",
  "count": 1,
  "decisions": [
    {
      "id": "d702e971f5cd1",
      "kind": "choice",
      "title": "Tabs or a sidebar for the settings?",
      "waiting": 2,
      "count": 2,
      "state": "open",
      "muted": true
    }
  ]
}
  (of each of `decisions`, left out here: key, space, sessions, why, options, pick, prs, question_id, task, first_ms, last_ms)
```
```
GET /v1/state
→ 200 {"decisions_count": 0}
  (left out here: version, updated_ms, pid, running, mode, mic, armed, transcribing, speaking, muted, voice_on_mac, announcing, brain, brain_app, hearing, heard, heard_ms, dictated, dictated_ms, said, said_ms, alerts, missed_while_muted, agents, error, talk_key, voice, stt_model, lan, lan_hosts, tailscale, phone, screenshots, questions, question_policy, behaviors, tell_level, phase, brains, brain_model, work, audio_route)
```
```
POST /v1/spaces/fix-login/mute
{"on": false}
→ 200 {"space": "fix-login", "mute": false}
  (left out here: mission)
```

A spoken answer is read against its options first, then the answer is sent. Both sessions wait on it, so both get it:

```
POST /v1/decisions/d702e971f5cd1/interpret
{"text": "the first one"}
→ 200 {"tab": 0, "choice": {"options": [0], "other": null}, "answer": {"choice": 0}}
```
```
POST /v1/decisions/d702e971f5cd1/answer
{"choice": 0}
→ 200 {
  "ok": true,
  "id": "d702e971f5cd1",
  "state": "answered",
  "answer": "Tabs",
  "sent": [
    {"session": "t3", "name": "fix login eyzflow", "queue_id": 3, "sent": true},
    {"session": "t4", "name": "fix login mediaroom", "queue_id": 4, "sent": true}
  ],
  "errors": []
}
```
```
what the two sessions' terminals were sent (the sandbox's stand-in for Herdr logs it):
  t3 ← "Use tabs for the settings, as in the mock."
  t4 ← "Use tabs for the settings, as in the mock."
```

Answered already (another tap, another screen): a conflict that says how it ended. And an id nobody has:

```
POST /v1/decisions/d702e971f5cd1/answer
{"choice": 1}
→ 409 {"error": {"code": "closed", "message": "d702e971f5cd1 is answered already", "state": "answered"}}
```
```
GET /v1/decisions
→ 200 {"count": 0, "decisions": []}
  (left out here: space)
```
```
POST /v1/decisions/d000000000000/answer
{"choice": 0}
→ 404 {"error": {"code": "no_decision", "message": "no decision with that id"}}
```

One more, dismissed without an answer:

```
DELETE /v1/decisions/d272e5d4ee03c
→ 200 {"dismissed": "d272e5d4ee03c", "state": "dismissed"}
```

## 5. The sessions talk to each other: the space's thread

One session writes to the other, and gets an answer (each runs `tower queue` in its own terminal):

```
on the Mac$ tower queue t4 "Which port does the api listen on?"        (run in t3)
{
  "sent": true,
  "id": 5,
  "session": "t4",
  "name": "fix login mediaroom",
  "peer": true,
  "from": "t3"
}
```
```
on the Mac$ tower queue t3 "3001. The redirect goes through /auth/callback."        (run in t4)
{
  "sent": true,
  "id": 6,
  "session": "t3",
  "name": "fix login eyzflow",
  "peer": true,
  "from": "t4"
}
```
```
GET /v1/thread?space=fix-login&limit=1
→ 200 {
  "space": "fix-login",
  "messages": [
    {
      "id": "h23",
      "at": 1794369754024,
      "from": "t4",
      "from_name": "fix login mediaroom",
      "to": "t3",
      "to_name": "fix login eyzflow",
      "text": "3001. The redirect goes through /auth/callback.",
      "space": "fix-login",
      "delivered": true
    }
  ],
  "more": true
}
```
```
GET /v1/thread?space=fix-login&limit=1&before=h23
→ 200 {
  "space": "fix-login",
  "messages": [
    {
      "id": "h22",
      "at": 1794369749048,
      "from": "t3",
      "from_name": "fix login eyzflow",
      "to": "t4",
      "to_name": "fix login mediaroom",
      "text": "Which port does the api listen on?",
      "space": "fix-login",
      "delivered": true
    }
  ],
  "more": false
}
```

## 6. Asking: the tower, and a space's own tower

```
POST /v1/ask
{"text": "What is left here?", "space": "fix-login"}
→ 202 {"sent": "What is left here?", "attachments": [], "space": "fix-login"}
```
```
POST /v1/ask
{"text": "What is everyone doing?"}
→ 202 {"sent": "What is everyone doing?", "attachments": []}
```
```
POST /v1/ask
  --form text=And the tests?
  --form space=fix-login
→ 202 {"sent": "And the tests?", "attachments": [], "space": "fix-login"}
```
```
POST /v1/ask
{"text": "hello?", "space": "No such space"}
→ 404 {"error": {"code": "no_space", "message": "no space called \"No such space\""}}
```
```
where each went (what the brains' terminals were sent, cut short):
  t5 ← "[voice] What is left here?"
  tower ← "[voice] What is everyone doing?"
  t5 ← "[voice] And the tests?"
```

The brains in this sandbox are stand-ins that never answer, so two answers are written into the history by hand here, as the brains' hooks would write them. Then the chat, whole and by conversation:

```
GET /v1/chat?limit=6
→ 200 {
  "messages": [
    {
      "id": "h14",
      "role": "event",
      "kind": "alert",
      "text": "fix login eyzflow needs you: Deploy the redirect fix to staging?",
      "source": "tower",
      "space": "fix-login"
    },
    {
      "id": "h24",
      "role": "user",
      "kind": "request",
      "text": "What is left here?",
      "source": "phone:d_7463a95d",
      "space": "fix-login"
    },
    {
      "id": "h25",
      "role": "user",
      "kind": "request",
      "text": "What is everyone doing?",
      "source": "phone:d_7463a95d",
      "space": null
    },
    {
      "id": "h26",
      "role": "user",
      "kind": "request",
      "text": "And the tests?",
      "source": "phone:d_7463a95d",
      "space": "fix-login"
    },
    {
      "id": "h27",
      "role": "tower",
      "kind": "answer",
      "text": "Two tasks: the redirect itself, and its test.",
      "source": "tower",
      "space": "fix-login"
    },
    {
      "id": "h28",
      "role": "tower",
      "kind": "answer",
      "text": "Two sessions are working on the login redirect.",
      "source": "tower",
      "space": null
    }
  ],
  "more": true
}
  (of each of `messages`, left out here: at, updated_at, attachments, screenshots, session, name, status)
```
```
GET /v1/chat?space=fix-login
→ 200 {
  "messages": [
    {
      "id": "h24",
      "role": "user",
      "kind": "request",
      "text": "What is left here?",
      "source": "phone:d_7463a95d",
      "space": "fix-login"
    },
    {
      "id": "h26",
      "role": "user",
      "kind": "request",
      "text": "And the tests?",
      "source": "phone:d_7463a95d",
      "space": "fix-login"
    },
    {
      "id": "h27",
      "role": "tower",
      "kind": "answer",
      "text": "Two tasks: the redirect itself, and its test.",
      "source": "tower",
      "space": "fix-login"
    }
  ],
  "more": false
}
  (of each of `messages`, left out here: at, updated_at, attachments, screenshots)
```
```
GET /v1/chat?main=1&limit=3
→ 200 {
  "messages": [
    {
      "id": "h14",
      "role": "event",
      "kind": "alert",
      "text": "fix login eyzflow needs you: Deploy the redirect fix to staging?",
      "source": "tower",
      "space": "fix-login"
    },
    {
      "id": "h25",
      "role": "user",
      "kind": "request",
      "text": "What is everyone doing?",
      "source": "phone:d_7463a95d",
      "space": null
    },
    {
      "id": "h28",
      "role": "tower",
      "kind": "answer",
      "text": "Two sessions are working on the login redirect.",
      "source": "tower",
      "space": null
    }
  ],
  "more": true
}
  (of each of `messages`, left out here: at, updated_at, attachments, screenshots, session, name, status)
```

## 7. The Record

```
GET /v1/history?space=fix-login&kind=peer,raised,decision&limit=6
→ 200 {
  "entries": [
    {
      "at": 1794369737383,
      "kind": "raised",
      "session": "t4",
      "name": "fix login mediaroom",
      "space": "fix-login",
      "text": "tabs or a sidebar for the settings",
      "id": "d702e971f5cd1",
      "record": true,
      "why": "design"
    },
    {
      "at": 1794369740071,
      "kind": "decision",
      "session": "t3",
      "name": "fix login eyzflow",
      "space": "fix-login",
      "status": "answered",
      "text": "Tabs or a sidebar for the settings? -> Tabs",
      "id": "d702e971f5cd1",
      "record": true,
      "why": "design"
    },
    {
      "at": 1794369747575,
      "kind": "raised",
      "session": "t3",
      "name": "fix login eyzflow",
      "space": "fix-login",
      "text": "Deploy the redirect fix to staging?",
      "id": "d272e5d4ee03c",
      "why": "deploy"
    },
    {
      "at": 1794369748916,
      "kind": "decision",
      "session": "t3",
      "name": "fix login eyzflow",
      "space": "fix-login",
      "status": "dismissed",
      "text": "Deploy the redirect fix to staging?",
      "id": "d272e5d4ee03c",
      "record": true,
      "why": "deploy"
    },
    "… 2 more"
  ]
}
```
```
GET /v1/history?space=fix-login&limit=200
→ 200, 26 entries; by kind: alert 4, answer 1, decision 2, job 6, peer 2, queue 4, raised 3, request 2, watch 2
```

## 8. How much it tells, and the brains' model

```
GET /v1/tell
→ 200 {"level": "decisions"}
```
```
POST /v1/tell
{"level": "results"}
→ 200 {"level": "results", "was": "decisions"}
```
```
POST /v1/tell
{"level": "loud"}
→ 400 {"error": {"code": "bad_level", "message": "level is decisions, results or everything"}}
```
```
POST /v1/tell
{"level": "decisions"}
→ 200 {"level": "decisions", "was": "results"}
```
```
GET /v1/model
→ 200 {
  "program": "claude",
  "model": "claude-opus-5-5",
  "effort": "high",
  "running": [
    {
      "brain": "tower",
      "space": null,
      "address": "t1",
      "model": "claude-opus-5-5",
      "effort": "high",
      "restart": false
    },
    {
      "brain": "tower-fix-login",
      "space": "fix-login",
      "address": "t5",
      "model": "claude-opus-5-5",
      "effort": "high",
      "restart": false
    }
  ],
  "restart_needed": false,
  "says": "The tower's brain runs on Opus 5.5 at high effort."
}
  (left out here: choices, locked_by_args, locked, restarted)
```
```
POST /v1/model
{"model": "claude-fable-5-2", "effort": "xhigh"}
→ 200 {
  "model": "claude-fable-5-2",
  "effort": "xhigh",
  "running": [
    {
      "brain": "tower",
      "space": null,
      "address": "t1",
      "model": "claude-opus-5-5",
      "effort": "high",
      "restart": true
    },
    {
      "brain": "tower-fix-login",
      "space": "fix-login",
      "address": "t5",
      "model": "claude-opus-5-5",
      "effort": "high",
      "restart": true
    }
  ],
  "restart_needed": true,
  "says": "The tower's brain runs on Opus 5.5 at high effort. Fable 5.2 at xhigh effort applies when it restarts.",
  "restarted": []
}
  (left out here: program, choices, locked_by_args, locked)
```
```
POST /v1/model
{"effort": "extreme"}
→ 400 {"error": {"code": "bad_effort", "message": "effort is low, medium, high, xhigh, max: not \"extreme\""}}
```
```
POST /v1/model
{"restart": true}
→ 200 {
  "model": "claude-fable-5-2",
  "effort": "xhigh",
  "restart_needed": true,
  "says": "The tower's brain runs on Opus 5.5 at high effort. Fable 5.2 at xhigh effort applies when it restarts.",
  "restarted": ["tower", "tower-fix-login"]
}
  (left out here: program, choices, running, locked_by_args, locked)
```
```
GET /v1/state
→ 200 {
  "decisions_count": 0,
  "tell_level": "decisions",
  "brain_model": {
    "program": "claude",
    "model": "claude-fable-5-2",
    "effort": "xhigh",
    "choices": {
      "models": [
        {"id": "claude-fable-5-2", "label": "Fable 5.2"},
        {"id": "claude-opus-5-5", "label": "Opus 5.5"},
        {"id": "claude-sonnet-5-5", "label": "Sonnet 5.5"},
        {"id": "claude-haiku-5-5", "label": "Haiku 5.5"}
      ],
      "efforts": ["low", "medium", "high", "xhigh", "… 1 more"]
    },
    "running": [
      {
        "brain": "tower",
        "space": null,
        "address": "t6",
        "model": "claude-fable-5-2",
        "effort": "xhigh",
        "restart": false
      },
      {
        "brain": "tower-fix-login",
        "space": "fix-login",
        "address": "t7",
        "model": "claude-fable-5-2",
        "effort": "xhigh",
        "restart": false
      }
    ],
    "restart_needed": false,
    "locked_by_args": false,
    "locked": {"model": null, "effort": null},
    "says": "The tower's brain runs on Fable 5.2 at xhigh effort."
  },
  "brains": [
    {
      "name": "tower",
      "space": null,
      "address": "t6",
      "status": "idle",
      "model": "claude-fable-5-2",
      "effort": "xhigh"
    },
    {
      "name": "tower-fix-login",
      "space": "fix-login",
      "address": "t7",
      "status": "idle",
      "model": "claude-fable-5-2",
      "effort": "xhigh"
    }
  ]
}
  (left out here: version, updated_ms, pid, running, mode, mic, armed, transcribing, speaking, muted, voice_on_mac, announcing, brain, brain_app, hearing, heard, heard_ms, dictated, dictated_ms, said, said_ms, alerts, missed_while_muted, agents, error, talk_key, voice, stt_model, lan, lan_hosts, tailscale, phone, screenshots, questions, question_policy, behaviors, phase, work, audio_route)
```

## 9. Closing the space

One of its sessions is at work (the stand-in is told so), so a close is refused, naming it:

```
POST /v1/spaces/fix-login/close
{}
→ 409 {
  "error": {
    "code": "busy",
    "message": "\"fix-login\" isn't closed: fix-login-eyzflow (t3) is working. Wait for them, or send {\"force\": true} (their work in progress is cut off).",
    "sessions": [{"address": "t3", "name": "fix-login-eyzflow", "status": "working"}]
  }
}
```

Closed anyway. Again the answer is an action, and its receipt comes when the sessions have ended and the worktrees are gone:

```
POST /v1/spaces/fix-login/close
{"force": true}
→ 202 {
  "action": {
    "id": "act_e7cd9e2e",
    "kind": "close",
    "space": "fix-login",
    "state": "running",
    "plan": null,
    "receipt": null,
    "error": null,
    "by": "phone:d_7463a95d",
    "started_ms": 1794369771375,
    "finished_ms": null
  }
}
```
```
GET /v1/actions/act_e7cd9e2e
→ 200 {
  "action": {
    "id": "act_e7cd9e2e",
    "kind": "close",
    "space": "fix-login",
    "state": "done",
    "plan": null,
    "receipt": {
      "closed": "fix-login",
      "sessions_ended": [{"address": "t3", "name": "fix-login-eyzflow"}, {"address": "t4", "name": "fix-login-mediaroom"}],
      "sessions_left": [],
      "herdr_space": "closed",
      "worktrees_removed": [
        "/sandbox/work/eyz/.tower-worktrees/fix-login/eyzflow",
        "/sandbox/work/eyz/.tower-worktrees/fix-login/mediaroom"
      ],
      "kept_worktrees": [],
      "requests_closed": ["a1"],
      "tower_ended": ["tower-fix-login"],
      "report": ""
    },
    "error": null,
    "by": "phone:d_7463a95d",
    "started_ms": 1794369771375,
    "finished_ms": 1794369772240
  }
}
```
```
GET /v1/missions
→ 200 {"spaces": [], "quiet": [], "closed_today": ["fix-login"]}
```

## 10. What came over the event stream meanwhile

```
event: hello      {"api": 1, "device": "d_7463a95d", "voice": false, "features": ["quiet"]}
event: state      {"version": 4, "phase": "idle", "decisions_count": 0, "tell_level": "decisions"}   (the first; no decisions, missions or threads in it; 43 keys in all)
event: decisions  count=0  []
event: missions   [] closed_today=[]
event: action     act_3cbc0b4e launch "fix-login" state=running
event: action     act_3cbc0b4e launch "fix-login" state=running sessions=["t3", null]
event: action     act_3cbc0b4e launch "fix-login" state=running sessions=["t3", "t4"]
event: action     act_3cbc0b4e launch "fix-login" state=done sessions=["t3", "t4"]
event: missions   [fix-login open follow=false mute=false needs=0 sessions=2 tower=none] closed_today=[]
event: missions   [fix-login open follow=false mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: missions   [fix-login open follow=false mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: missions   [fix-login open follow=true mute=true needs=0 sessions=2 tower=ready] closed_today=[]
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: decisions  count=1  [d702e971f5cd1 choice count=1 last_ms=1794369735747 waiting=1 state=open]
event: missions   [fix-login open follow=true mute=false needs=1 sessions=2 tower=ready] closed_today=[]
event: chat       op=add h4 role=event kind=alert space="fix-login" text="fix login eyzflow needs you: Tabs or a sidebar for the setti"
event: decisions  count=1  [d702e971f5cd1 choice count=2 last_ms=1794369737382 waiting=2 state=open]
event: decisions  count=0  [d702e971f5cd1 choice count=2 last_ms=1794369737382 waiting=2 state=open muted]
event: missions   [fix-login open follow=true mute=true needs=1 sessions=2 tower=ready] closed_today=[]
event: decisions  count=1  [d702e971f5cd1 choice count=2 last_ms=1794369737382 waiting=2 state=open]
event: missions   [fix-login open follow=true mute=false needs=1 sessions=2 tower=ready] closed_today=[]
event: decisions  count=0  []
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: decisions  count=1  [d272e5d4ee03c raised count=1 last_ms=1794369747574 waiting=1 state=open]
event: missions   [fix-login open follow=true mute=false needs=1 sessions=2 tower=ready] closed_today=[]
event: chat       op=add h14 role=event kind=alert space="fix-login" text="fix login eyzflow needs you: Deploy the redirect fix to stag"
event: decisions  count=0  []
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: thread     {
                    "space": "fix-login",
                    "message": {
                      "id": "h22",
                      "at": 1794369749048,
                      "from": "t3",
                      "from_name": "fix login eyzflow",
                      "to": "t4",
                      "to_name": "fix login mediaroom",
                      "text": "Which port does the api listen on?",
                      "space": "fix-login",
                      "delivered": true
                    }
                  }
event: thread     {
                    "space": "fix-login",
                    "message": {
                      "id": "h23",
                      "at": 1794369754024,
                      "from": "t4",
                      "from_name": "fix login mediaroom",
                      "to": "t3",
                      "to_name": "fix login eyzflow",
                      "text": "3001. The redirect goes through /auth/callback.",
                      "space": "fix-login",
                      "delivered": true
                    }
                  }
event: chat       op=add h24 role=user kind=request space="fix-login" text="What is left here?"
event: chat       op=add h25 role=user kind=request space=null text="What is everyone doing?"
event: chat       op=add h26 role=user kind=request space="fix-login" text="And the tests?"
event: chat       op=add h27 role=tower kind=answer space="fix-login" text="Two tasks: the redirect itself, and its test."
event: chat       op=add h28 role=tower kind=answer space=null text="Two sessions are working on the login redirect."
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=off] closed_today=[]
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=ready] closed_today=[]
event: action     act_e7cd9e2e close "fix-login" state=running
event: missions   [fix-login open follow=true mute=false needs=0 sessions=2 tower=off] closed_today=[]
event: action     act_e7cd9e2e close "fix-login" state=done closed="fix-login"
event: missions   [] closed_today=["fix-login"]
```

Events of each kind over the whole walk: action 6, chat 7, decisions 8, hello 1, history 29, missions 17, state 18, thread 2.

## What this walk did not make

- **A question's decision** (`kind` `question` or `permission`): it needs a
  session that really shows a dialog or ends its turn on a question, which
  the stand-in sessions don't. Its answer (`{"choice"}`, `{"other"}`,
  `{"answers"}`, the form with files), `interpret` and `voice` by decision
  id, and the `409 closed` when its session is gone, are covered by the
  tests (`cargo test lan`: `the_queue_is_read_answered_and_dismissed`).
- **A merge's decision**: it needs GitHub. Its shape (`prs[]`), the
  `400 merge` on an answer and its dismissal are in the same test; the
  merge itself is the PR calls' own test (`a_phone_merges_a_pr_in_two_steps`).
- **A failed task's decision**: answered in the tests
  (`an_answer_goes_to_the_sessions_that_wait`).
- **Voice**: the sandbox tower has no Whisper, so `…/voice` wasn't walked.
- **A launch whose sessions fail to open**, and a slow launch watched step
  by step: in the tests (`a_launch_answers_at_once_and_says_each_step`).
  Here the launch was over about a second and a half after it was sent,
  because the stand-in sessions open at once; on a real tower each session
  takes seconds, sometimes minutes.
- **Names with spaces, slashes and letters outside ASCII** in a path: the
  space here is called `fix-login`. The tests set the goal of `B2C support`
  and of `Q&A / ünï` (`a_space_has_a_goal_its_switches_and_an_end`).
