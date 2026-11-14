//! What the Quiet Tower adds to the phone API: the queue of decisions, the
//! spaces as missions, the threads between their sessions, launching and
//! closing a space, how much the tower tells, and the brains' model. The
//! contract is in `docs/mobile-api.md` ("The Quiet Tower").
//!
//! Every call here goes through `tower::Tower`, which the daemon answers
//! with what its socket's handlers and the CLI's commands call: a phone and
//! the Mac's windows do the same thing.

use std::thread;

use serde_json::{Value, json};

use super::http::{Request, Response};
use super::tower::Tower;
use super::{Lan, attach_tab_files, auth, clean_json, clean_text, multipart, question_code, starting};
use crate::daemon::decisions::Refused;
use crate::questions::{Answer, Opt, Tab};

/// A call's answer, or the error to send instead.
type Answered = Result<Response, Response>;

impl Lan {
    /// The Quiet Tower's calls. None: not one of them.
    pub(super) fn quiet(&self, req: &Request, device: &auth::Device, seg: &[&str]) -> Option<Response> {
        let tower = self.tower();
        let t = || tower.as_deref().ok_or_else(starting);
        let q = |k: &str| req.query.get(k).map(String::as_str).map(str::trim).filter(|v| !v.is_empty());
        let r: Answered = match (req.method.as_str(), seg) {
            // What only the user can decide, in queue order.
            ("GET", ["decisions"]) => t().map(|t| Response::json(200, &t.decisions(q("space"), q("all") == Some("1")))),
            ("GET", ["decisions", id]) => t().and_then(|t| decision(t, id)).map(|d| Response::json(200, &json!({ "decision": d }))),
            ("POST", ["decisions", id, "answer"]) => t().and_then(|t| self.answer_decision(t, req, id)),
            ("DELETE", ["decisions", id]) => t().and_then(|t| dismiss_decision(t, id)),
            // On hold (`{"reason": "<the user's words>"}`, may be empty), and back.
            ("POST", ["decisions", id, "hold"]) => t().and_then(|t| hold_decision(t, req, device, id, false)),
            ("POST", ["decisions", id, "unhold"]) => t().and_then(|t| hold_decision(t, req, device, id, true)),
            // The answer just given, made a standing rule: asked of the tower in the user's words.
            ("POST", ["decisions", id, "rule"]) => t().and_then(|t| self.make_rule(t, req, device, id)),
            // What the tower opened lately, and taking one back.
            ("GET", ["acts"]) => t().map(|t| Response::json(200, &t.acts())),
            ("GET", ["acts", id]) => t().and_then(|t| act(t, id)).map(|a| Response::json(200, &json!({ "act": a }))),
            ("POST", ["acts", id, "undo"]) => t().and_then(|t| undo_act(t, req, device, id)),
            // A spoken answer for a decision: text (from /v1/transcribe), or a WAV take.
            ("POST", ["decisions", id, "interpret"]) => t().and_then(|t| {
                let body = req.json().unwrap_or_default();
                let tab = body["tab"].as_u64().unwrap_or(0) as usize;
                let (tab_of, question) = self.decision_tab(t, id, tab)?;
                Ok(Response::json(200, &spoken(tab, None, body["text"].as_str().unwrap_or(""), &tab_of, question)))
            }),
            ("POST", ["decisions", id, "voice"]) => t().and_then(|t| {
                let tab = q("tab").and_then(|n| n.parse::<usize>().ok()).unwrap_or(0);
                let (tab_of, question) = self.decision_tab(t, id, tab)?;
                let heard = self.voice(req, false, device, None);
                if heard.status != 200 {
                    return Err(heard);
                }
                let text = serde_json::from_slice::<Value>(&heard.body).unwrap_or_default()["heard"].as_str().unwrap_or("").to_string();
                Ok(Response::json(200, &spoken(tab, Some(&text), &text, &tab_of, question)))
            }),
            // Every space as a mission.
            ("GET", ["missions"]) => t().map(|t| Response::json(200, &t.missions())),
            ("POST" | "PUT", ["spaces", space, "goal"]) => t().and_then(|t| self.set_goal(t, req, space)),
            ("POST" | "PUT", ["spaces", space, what @ ("follow" | "mute")]) => t().and_then(|t| self.set_switch(t, req, space, what)),
            ("POST", ["spaces", space, "close"]) => t().and_then(|_| self.close_space(req, device, space, false)),
            ("POST", ["spaces", space, "resolve"]) => t().and_then(|_| self.close_space(req, device, space, true)),
            // What closing or resolving it would end and remove (nothing changes).
            ("GET", ["spaces", space, "closing"]) => t().and_then(|t| {
                let name = self.space_named(space)?;
                t.closing(&name).map(|v| Response::json(200, &v)).map_err(|e| Response::error(503, "herdr", &format!("{e:#}")))
            }),
            // What the sessions of a space wrote to each other, newest last, in pages.
            ("GET", ["thread"]) => {
                let limit = q("limit").and_then(|l| l.parse::<usize>().ok()).unwrap_or(50).min(200);
                let (messages, more) = crate::peers::page(&self.lines(), q("space"), q("before"), limit);
                Ok(Response::json(200, &json!({ "space": q("space"), "messages": messages, "more": more })))
            }
            // What a space is launched with, and launching one.
            ("GET", ["setups"]) => {
                t().and_then(|t| t.setups().map(|v| Response::json(200, &v)).map_err(|e| Response::error(503, "config", &format!("{e:#}"))))
            }
            ("POST", ["launch"]) => t().and_then(|_| self.launch(req, device)),
            ("GET", ["actions"]) => {
                Ok(Response::json(200, &json!({ "actions": self.actions.all().iter().map(|a| a.view()).collect::<Vec<_>>() })))
            }
            ("GET", ["actions", id]) => match self.actions.get(id) {
                Some(a) => Ok(Response::json(200, &json!({ "action": a.view() }))),
                None => Err(Response::error(404, "no_action", "no launch or close with that id (they are kept until the tower restarts)")),
            },
            // How much the tower tells the user, and what it says out loud.
            ("GET", ["tell"]) => t().and_then(|t| {
                t.tell(None, None).map(|v| Response::json(200, &v)).map_err(|e| Response::error(503, "config", &format!("{e:#}")))
            }),
            ("POST" | "PUT", ["tell"]) => t().and_then(|t| {
                let body = req.json().unwrap_or_default();
                let level = match &body["level"] {
                    Value::Null => None,
                    Value::String(l) => match crate::tell::Level::parse(l) {
                        Some(l) => Some(l),
                        None => return Err(Response::error(400, "bad_level", "level is decisions, results or everything")),
                    },
                    _ => return Err(Response::error(400, "bad_level", "level is decisions, results or everything")),
                };
                let voice = match &body["voice"] {
                    Value::Null => None,
                    Value::String(v) => match crate::tell::Voice::parse(v) {
                        Some(v) => Some(v),
                        None => return Err(Response::error(400, "bad_voice", "voice is off, decisions or replies")),
                    },
                    _ => return Err(Response::error(400, "bad_voice", "voice is off, decisions or replies")),
                };
                if level.is_none() && voice.is_none() {
                    return Err(Response::error(400, "bad_level", "send level (decisions, results or everything) or voice (off, decisions or replies)"));
                }
                t.tell(level.map(|l| l.word()), voice.map(|v| v.word()))
                    .map(|v| Response::json(200, &v))
                    .map_err(|e| Response::error(503, "save", &format!("{e:#}")))
            }),
            // The model the tower's brains run on.
            ("GET", ["model"]) => t().and_then(|t| {
                t.model(None, None, false).map(|v| Response::json(200, &v)).map_err(|e| Response::error(503, "config", &format!("{e:#}")))
            }),
            ("POST" | "PUT", ["model"]) => t().and_then(|t| set_model(t, req)),
            _ => return None,
        };
        Some(r.unwrap_or_else(|e| e))
    }

    /// `POST /v1/decisions/{id}/answer`: `{"choice": n}`, `{"other":
    /// "text"}`, or a question's answers as `/v1/questions/{id}/answer`
    /// takes them (`{answers, submit}`; multipart with that JSON as a
    /// `json` part and files for tab N's typed answer as parts named `tab<N>`).
    fn answer_decision(&self, t: &dyn Tower, req: &Request, id: &str) -> Answered {
        let d = decision(t, id)?;
        let (mut body, files): (Value, Vec<multipart::Part>) = match req.header("content-type").and_then(multipart::boundary) {
            Some(b) => {
                let parts = multipart::parse(&req.body, &b).map_err(|e| Response::error(400, "bad_multipart", &format!("{e:#}")))?;
                let (json_part, rest): (Vec<_>, Vec<_>) = parts.into_iter().partition(|p| p.name == "json");
                let Some(j) = json_part.into_iter().next() else {
                    return Err(Response::error(400, "no_json", "a multipart answer needs a `json` part with the answer"));
                };
                (serde_json::from_slice(j.data).map_err(|e| Response::error(400, "bad_json", &format!("the `json` part: {e}")))?, rest)
            }
            None => (req.json().map_err(|e| Response::error(400, "bad_json", &format!("{e}")))?, vec![]),
        };
        // What is typed goes into a terminal: no control characters.
        clean_json(&mut body);
        if !files.is_empty() {
            // Files go with a question's typed answers: saved only for a set that is open.
            let Some(set) = d["question_id"].as_str().and_then(|q| self.ctx.asks.get(q)) else {
                return Err(Response::error(400, "no_files", "only a question's typed answer takes files"));
            };
            let given = if body.is_array() { Some(body.clone()) } else { body.get("answers").filter(|a| a.is_array()).cloned() };
            let mut answers: Vec<Answer> = match (given, body["other"].as_str()) {
                (Some(a), _) => serde_json::from_value(a).map_err(|e| Response::error(400, "bad_answers", &format!("answers: {e}")))?,
                // The one question's typed answer, in the user's words.
                (None, Some(text)) if set.tabs.len() <= 1 => vec![Answer { tab: 0, choice: json!({ "other": text }), attachments: vec![] }],
                _ => {
                    return Err(Response::error(
                        400,
                        "bad_answers",
                        "files go with typed answers: send `answers` (or `other`, for one question)",
                    ));
                }
            };
            if answers.iter().any(|a| a.tab >= set.tabs.len()) {
                return Err(Response::error(400, "bad_tab", "no such tab"));
            }
            attach_tab_files(&mut answers, &files)?;
            body = json!({ "answers": answers, "submit": body["submit"].as_bool().unwrap_or(true) });
        }
        let submit = body.get("submit").and_then(Value::as_bool).unwrap_or(true);
        match t.answer(id, &body, submit) {
            Ok(v) => Ok(Response::json(200, &v)),
            Err(r) => Err(refused(t, id, r)),
        }
    }

    /// `POST /v1/decisions/{id}/rule`: asks the tower of the decision's
    /// space, as a typed request would, to make the answer a standing rule
    /// (`{"answer": "<what it was answered with>"}` when the app knows it
    /// better than the tower: a question answered step by step).
    fn make_rule(&self, t: &dyn Tower, req: &Request, device: &auth::Device, id: &str) -> Answered {
        decision(t, id)?;
        let mut body = req.json().unwrap_or_default();
        clean_json(&mut body);
        let (text, space) = t.rule(id, body["answer"].as_str()).map_err(|e| match e {
            Refused::Unknown(_) => Response::error(404, "no_decision", "no decision with that id"),
            Refused::Bad(why) => Response::error(409, "not_answered", &why),
            other => Response::error(409, "no_rule", &other.to_string()),
        })?;
        let source = format!("phone:{}", device.id);
        t.ask(&text, super::heard_on_phone(device), &source, space.as_deref())
            .map_err(|e| Response::error(503, "brain", &crate::brain::spoken_error(&format!("{e:#}"))))?;
        eprintln!("[phone] {} makes {id} a rule{}", device.name, space.as_deref().map(|s| format!(" in {s}")).unwrap_or_default());
        Ok(Response::json(202, &json!({ "sent": text, "space": space })))
    }

    /// The one question a spoken answer is for: tab `tab` of the decision's
    /// question set; for a decision that is no question, its options as the
    /// one tab. And whether it is a question's.
    fn decision_tab(&self, t: &dyn Tower, id: &str, tab: usize) -> Result<(Tab, bool), Response> {
        let d = decision(t, id)?;
        over(&d)?;
        if d["kind"] == "merge" {
            return Err(merge_error(&d));
        }
        if let Some(set) = d["question_id"].as_str().and_then(|q| self.ctx.asks.get(q)) {
            return set.tabs.get(tab).cloned().map(|t| (t, true)).ok_or_else(|| Response::error(400, "bad_tab", "no such tab"));
        }
        if tab != 0 {
            return Err(Response::error(400, "bad_tab", "no such tab"));
        }
        let options = d["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|o| Opt { label: o["label"].as_str().unwrap_or("").to_string(), ..Default::default() })
            .collect();
        Ok((Tab { question: d["title"].as_str().unwrap_or("").to_string(), options, allows_other: true, ..Default::default() }, false))
    }

    /// `POST /v1/spaces/{space}/goal`: `goal` ("" clears it), `done_when`
    /// and `standing`, each when given.
    fn set_goal(&self, t: &dyn Tower, req: &Request, space: &str) -> Answered {
        let name = self.space_named(space)?;
        let body = req.json().map_err(|e| Response::error(400, "bad_json", &format!("{e}")))?;
        let word = |k: &str| match &body[k] {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(clean_text(s).split_whitespace().collect::<Vec<_>>().join(" "))),
            _ => Err(Response::error(400, "bad_goal", &format!("{k} is a string"))),
        };
        let (goal, done_when) = (word("goal")?, word("done_when")?);
        let standing = match &body["standing"] {
            Value::Null => None,
            Value::Bool(on) => Some(*on),
            _ => return Err(Response::error(400, "bad_goal", "standing is true or false")),
        };
        if goal.is_none() && done_when.is_none() && standing.is_none() {
            return Err(Response::error(400, "bad_goal", "send goal, done_when or standing"));
        }
        let mut v =
            t.goal(&name, goal.as_deref(), done_when.as_deref(), standing).map_err(|e| Response::error(503, "save", &format!("{e:#}")))?;
        v["mission"] = mission(t, &name);
        Ok(Response::json(200, &v))
    }

    /// `POST /v1/spaces/{space}/follow` and `/mute`: `{"on": bool}`.
    fn set_switch(&self, t: &dyn Tower, req: &Request, space: &str, what: &str) -> Answered {
        let name = self.space_named(space)?;
        let Some(on) = req.json().unwrap_or_default()["on"].as_bool() else {
            return Err(Response::error(400, "bad_on", "send {\"on\": true} or {\"on\": false}"));
        };
        let done = if what == "mute" { t.mute(&name, on) } else { t.follow(&name, on) };
        done.map_err(|e| Response::error(503, "save", &format!("{e:#}")))?;
        Ok(Response::json(200, &json!({ "space": name, what: on, "mission": mission(t, &name) })))
    }

    /// `POST /v1/spaces/{space}/close`, and `…/resolve` (`resolve`: its goal
    /// is achieved, then closed the same way; only a space with a goal):
    /// refused while a session there is at work or waits for the user,
    /// unless `force`. Ending its sessions and removing its worktrees takes
    /// a while: it is an action (`close` or `resolve`).
    fn close_space(&self, req: &Request, device: &auth::Device, space: &str, resolve: bool) -> Answered {
        let Some(tower) = self.tower() else { return Err(starting()) };
        let name = self.space_named(space)?;
        let force = req.json().unwrap_or_default()["force"].as_bool().unwrap_or(false);
        if resolve && mission(tower.as_ref(), &name)["goal"].as_str().is_none_or(|g| g.trim().is_empty()) {
            return Err(Response::error(409, "no_goal", &format!("\"{name}\" has no goal to resolve: close it instead")));
        }
        if !force {
            let held = tower.holding(&name).map_err(|e| Response::error(503, "herdr", &format!("{e:#}")))?;
            if !held.is_empty() {
                let who: Vec<String> = held
                    .iter()
                    .map(|s| {
                        format!(
                            "{} ({}) is {}",
                            s["name"].as_str().unwrap_or(""),
                            s["address"].as_str().unwrap_or(""),
                            s["status"].as_str().unwrap_or("")
                        )
                    })
                    .collect();
                let message = format!(
                    "\"{name}\" isn't closed: {}. Wait for them, or send {{\"force\": true}} (their work in progress is cut off).",
                    who.join(", ")
                );
                return Err(Response::json(409, &json!({ "error": { "code": "busy", "message": message, "sessions": held } })));
            }
        }
        let by = format!("phone:{}", device.id);
        let same = |a: &super::tower::Action| (a.kind == "close" || a.kind == "resolve") && a.space.eq_ignore_ascii_case(&name);
        let kind = if resolve { "resolve" } else { "close" };
        let action = self
            .actions
            .begin(kind, &name, Value::Null, &by, same)
            .map_err(|running| under_way("closing", &format!("\"{name}\" is being closed already"), &running.id))?;
        eprintln!("[phone] {} {} \"{name}\"{} ({})", device.name, if resolve { "resolves" } else { "closes" }, if force { " (forced)" } else { "" }, action.id);
        let (actions, id) = (self.actions.clone(), action.id.clone());
        let spawned = thread::Builder::new().name("lan-close".into()).spawn(move || {
            let done = if resolve { tower.resolve(&name, force) } else { tower.close(&name, force) }.map_err(|e| format!("{e:#}"));
            if let Err(e) = &done {
                eprintln!("[phone] \"{name}\" wasn't closed: {e}");
            }
            actions.finish(&id, done);
        });
        if let Err(e) = spawned {
            self.actions.finish(&action.id, Err(format!("{e}")));
        }
        Ok(Response::json(202, &json!({ "action": action.view() })))
    }

    /// `POST /v1/launch`: `goal`, and `setup`, `name`, `dry_run` when given.
    /// The plan is made at once (what a dry run prints); opening its
    /// sessions takes a while: it is an action.
    fn launch(&self, req: &Request, device: &auth::Device) -> Answered {
        let Some(tower) = self.tower() else { return Err(starting()) };
        let body = req.json().map_err(|e| Response::error(400, "bad_json", &format!("{e}")))?;
        let word =
            |k: &str| body[k].as_str().map(|s| clean_text(s).split_whitespace().collect::<Vec<_>>().join(" ")).filter(|s| !s.is_empty());
        let Some(goal) = word("goal") else { return Err(Response::error(400, "empty", "send {\"goal\": \"<the goal, in a sentence>\"}")) };
        let plan = tower.plan(&goal, word("setup").as_deref(), word("name").as_deref()).map_err(|e| launch_error(&format!("{e:#}")))?;
        let planned = crate::launch::receipt(&plan, &[], None, true);
        if body["dry_run"].as_bool().unwrap_or(false) {
            return Ok(Response::json(200, &planned));
        }
        let by = format!("phone:{}", device.id);
        // One at a time: two at once would take the same name and the same ports.
        let action = self
            .actions
            .begin("launch", &plan.space, planned, &by, |a| a.kind == "launch")
            .map_err(|running| under_way("launching", &format!("\"{}\" is being launched: one at a time", running.space), &running.id))?;
        eprintln!("[phone] {} launches \"{}\" ({})", device.name, plan.space, action.id);
        let (actions, id) = (self.actions.clone(), action.id.clone());
        let spawned = thread::Builder::new().name("lan-launch".into()).spawn(move || {
            let mut each = |so_far: &Value| actions.change(&id, |a| a.receipt = so_far.clone());
            match tower.launch(&plan, &by, &mut each) {
                // Every session failed to open: its receipt says why, and it is no launch.
                Ok(r) if r["sessions"].as_array().is_some_and(|a| !a.is_empty() && a.iter().all(|s| !s["error"].is_null())) => {
                    actions.change(&id, |a| a.receipt = r.clone());
                    actions.finish(&id, Err(format!("no session of \"{}\" opened", plan.space)));
                }
                Ok(r) => actions.finish(&id, Ok(r)),
                Err(e) => actions.finish(&id, Err(format!("{e:#}"))),
            }
        });
        if let Err(e) = spawned {
            self.actions.finish(&action.id, Err(format!("{e}")));
        }
        Ok(Response::json(202, &json!({ "action": action.view() })))
    }
}

/// A decision by its id, as the queue carries it.
fn decision(t: &dyn Tower, id: &str) -> Result<Value, Response> {
    Some(id)
        .filter(|id| crate::decisions::is_id(id))
        .and_then(|id| t.decision(id))
        .ok_or_else(|| Response::error(404, "no_decision", "no decision with that id"))
}

/// An act by its id, as it is now.
fn act(t: &dyn Tower, id: &str) -> Result<Value, Response> {
    Some(id).filter(|id| crate::acts::is_id(id)).and_then(|id| t.act(id)).ok_or_else(|| Response::error(404, "no_act", "nothing the tower opened has that id"))
}

/// `POST /v1/acts/{id}/undo`: ends what that act opened (`{"force": true}`
/// though work would be lost). `409 held`, with the sessions and why, while
/// a session was given more work or its worktree holds changes.
fn undo_act(t: &dyn Tower, req: &Request, device: &auth::Device, id: &str) -> Answered {
    use crate::acts::Refused as No;
    act(t, id)?;
    let force = req.json().unwrap_or_default()["force"].as_bool().unwrap_or(false);
    match t.undo(id, force) {
        Ok(v) => {
            eprintln!("[phone] {} undid {id}{}", device.name, if force { " (forced)" } else { "" });
            Ok(Response::json(200, &v))
        }
        Err(No::Unknown(_)) => Err(Response::error(404, "no_act", "nothing the tower opened has that id")),
        Err(e @ No::Undone(_)) => Err(Response::json(409, &json!({ "error": { "code": "undone", "message": e.to_string(), "state": "undone" } }))),
        Err(No::Holds(list)) => {
            let sessions: Vec<Value> = list.iter().map(|h| json!({ "address": h.address, "name": h.name, "why": h.why })).collect();
            let message = format!("{} Send {{\"force\": true}} to undo it anyway.", No::Holds(list).to_string().split(" Undo it anyway").next().unwrap_or(""));
            Err(Response::json(409, &json!({ "error": { "code": "held", "message": message, "sessions": sessions } })))
        }
        Err(No::Failed(why)) => Err(Response::error(409, "cannot_undo", &why)),
    }
}

/// A decision that is over can't be answered: how it ended.
fn over(d: &Value) -> Result<(), Response> {
    match d["state"].as_str() {
        Some("open" | "answering") | None => Ok(()),
        Some(state) => Err(closed(d["id"].as_str().unwrap_or(""), state)),
    }
}

fn closed(id: &str, state: &str) -> Response {
    Response::json(409, &json!({ "error": { "code": "closed", "message": format!("{id} is {state} already"), "state": state } }))
}

/// A merge is its PR's own flow: which PRs, so the app can go there.
fn merge_error(d: &Value) -> Response {
    let prs: Vec<&str> = d["prs"].as_array().into_iter().flatten().filter_map(|p| p["key"].as_str()).collect();
    let message = "a merge isn't answered here: confirm and merge its PR (POST /v1/prs/{owner}/{repo}/{number}/confirm, then /merge), or dismiss the decision";
    Response::json(400, &json!({ "error": { "code": "merge", "message": message, "prs": prs } }))
}

/// Why an answer wasn't taken, as the phone hears it.
fn refused(t: &dyn Tower, id: &str, r: Refused) -> Response {
    let now = t.decision(id).unwrap_or_default();
    match &r {
        Refused::Unknown(_) => Response::error(404, "no_decision", "no decision with that id"),
        Refused::Closed { id, state } => closed(id, state.word()),
        Refused::Merge(_) => merge_error(&now),
        Refused::Bad(why) => Response::error(400, "bad_answer", why),
        Refused::Questions(errors) => match now["state"].as_str() {
            // Its question went meanwhile (answered in the session, the session gone): it is over.
            Some(state @ ("answered" | "dismissed" | "stopped")) => {
                Response::json(409, &json!({ "error": { "code": "closed", "message": r.to_string(), "state": state } }))
            }
            _ => Response::error(409, question_code(errors.first().map(|(_, why)| why.as_str()).unwrap_or("")), &r.to_string()),
        },
        Refused::Failed(why) => Response::error(409, "cannot_answer", why),
    }
}

/// `DELETE /v1/decisions/{id}`: out of the queue, nothing sent. One that is
/// over already says how it ended.
fn dismiss_decision(t: &dyn Tower, id: &str) -> Answered {
    decision(t, id)?;
    match t.dismiss(id) {
        Ok(v) => Ok(Response::json(200, &json!({ "dismissed": id, "state": v["state"] }))),
        Err(Refused::Unknown(_)) => Err(Response::error(404, "no_decision", "no decision with that id")),
        Err(e) => Err(Response::error(409, "cannot_dismiss", &e.to_string())),
    }
}

/// `POST /v1/decisions/{id}/hold` (`{"reason"}`) and `/unhold` (`{"why"}`):
/// the decision as it stands after. One that is over already: `409 closed`.
fn hold_decision(t: &dyn Tower, req: &Request, device: &auth::Device, id: &str, unhold: bool) -> Answered {
    decision(t, id)?;
    let body = req.json().unwrap_or_default();
    let words = body[if unhold { "why" } else { "reason" }].as_str().unwrap_or("");
    match t.hold(id, words, unhold, &format!("phone:{}", device.id)) {
        Ok(d) => Ok(Response::json(200, &json!({ "decision": d }))),
        Err(Refused::Unknown(_)) => Err(Response::error(404, "no_decision", "no decision with that id")),
        Err(Refused::Closed { state, .. }) => Err(Response::error(409, "closed", &format!("it is {} already", state.word()))),
        Err(e) => Err(Response::error(409, if unhold { "not_on_hold" } else { "cannot_hold" }, &e.to_string())),
    }
}

/// A spoken answer as a choice, to show before sending: `{tab, choice:
/// {options, other}}` (and `heard`, for a take). For a decision that is no
/// question, `answer` is the body to send to its `/answer`.
fn spoken(tab: usize, heard: Option<&str>, text: &str, of: &Tab, question: bool) -> Value {
    let p = crate::questions::interpret(text, of);
    let mut v = json!({ "tab": tab, "choice": { "options": p.options, "other": p.other } });
    if let Some(h) = heard {
        v["heard"] = json!(h);
    }
    if !question {
        v["answer"] = match (p.options.first(), &p.other) {
            (Some(n), _) => json!({ "choice": n }),
            (None, Some(words)) if !words.trim().is_empty() => json!({ "other": words }),
            _ => Value::Null,
        };
    }
    v
}

/// A space's row of the missions (null: it isn't listed, a closed one).
fn mission(t: &dyn Tower, name: &str) -> Value {
    let all = t.missions();
    all["spaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|m| m["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The same thing runs already: which action it is.
fn under_way(code: &str, message: &str, action: &str) -> Response {
    Response::json(409, &json!({ "error": { "code": code, "message": message, "action": action } }))
}

/// Why a launch can't be planned: no setups at all, a setup nobody has, or
/// something about it that doesn't fit (a repo that is no folder, an account).
fn launch_error(msg: &str) -> Response {
    if msg.starts_with("no setups yet") {
        Response::error(409, "no_setups", msg)
    } else if msg.starts_with("no setup ") {
        Response::error(404, "no_setup", msg)
    } else {
        Response::error(400, "bad_launch", msg)
    }
}

/// `POST /v1/model`: `model`, `effort`, `restart`, each when given.
fn set_model(t: &dyn Tower, req: &Request) -> Answered {
    let body = req.json().map_err(|e| Response::error(400, "bad_json", &format!("{e}")))?;
    let model = match &body["model"] {
        Value::Null => None,
        Value::String(m) => Some(crate::brains::parse_model(m).map_err(|e| Response::error(400, "bad_model", &format!("{e:#}")))?),
        _ => return Err(Response::error(400, "bad_model", "model is a string")),
    };
    let effort = match &body["effort"] {
        Value::Null => None,
        Value::String(e) => Some(crate::brains::parse_effort(e).map_err(|e| Response::error(400, "bad_effort", &format!("{e:#}")))?),
        _ => return Err(Response::error(400, "bad_effort", "effort is a string")),
    };
    let restart = body["restart"].as_bool().unwrap_or(false);
    if model.is_none() && effort.is_none() && !restart {
        return Err(Response::error(400, "bad_model", "send model, effort or restart"));
    }
    // A Codex brain's model comes from its own settings: nothing is changed.
    if (model.is_some() || effort.is_some()) && t.model(None, None, false).is_ok_and(|now| now["program"] == "codex") {
        return Err(Response::error(
            409,
            "codex",
            "the tower's brain is Codex here: its model comes from codex_model, codex_effort and codex_args in config.toml",
        ));
    }
    t.model(model.as_deref(), effort.as_deref(), restart)
        .map(|v| Response::json(200, &v))
        .map_err(|e| Response::error(503, "save", &format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Receiver, Sender, channel};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Result, bail};

    use super::super::hub::Hub;
    use super::super::tests::{call, call_raw, open, test_prs};
    use super::super::{Ctx, FEATURES};
    use super::*;
    use crate::daemon::asks::Asks;
    use crate::daemon::decisions::{Carry, Decisions};
    use crate::daemon::state::{Shared, State};
    use crate::daemon::tts::Mouth;
    use crate::decisions::Raise;
    use crate::launch::{Plan, World};
    use crate::missions::Mission;
    use crate::spaces;

    #[test]
    fn a_spoken_answer_becomes_what_to_send() {
        let options = |labels: &[&str]| Tab {
            options: labels.iter().map(|l| Opt { label: l.to_string(), ..Default::default() }).collect(),
            allows_other: true,
            ..Default::default()
        };
        // A raised choice: its options are the one tab, and the answer is ready to post.
        let v = spoken(0, None, "the second one", &options(&["Tabs", "A sidebar"]), false);
        assert_eq!(v, json!({ "tab": 0, "choice": { "options": [1], "other": null }, "answer": { "choice": 1 } }));
        let v = spoken(0, Some("neither, use a drawer"), "neither, use a drawer", &options(&["Tabs", "A sidebar"]), false);
        assert_eq!((v["heard"].as_str(), &v["answer"]), (Some("neither, use a drawer"), &json!({ "other": "neither, use a drawer" })));
        // No options (a plain raise): the words themselves.
        assert_eq!(spoken(0, None, "go ahead", &options(&[]), false)["answer"], json!({ "other": "go ahead" }));
        assert_eq!(spoken(0, None, "", &options(&[]), false)["answer"], Value::Null);
        // A question's tab: as `/v1/questions/{id}/interpret` answers, nothing more.
        let v = spoken(1, None, "the first", &options(&["Postgres", "SQLite"]), true);
        assert_eq!(v, json!({ "tab": 1, "choice": { "options": [0], "other": null } }));
    }

    #[test]
    fn a_launch_that_cant_be_planned_says_which_way() {
        let code = |msg: &str| {
            let r = launch_error(msg);
            (r.status, serde_json::from_slice::<Value>(&r.body).unwrap()["error"]["code"].as_str().unwrap().to_string())
        };
        // As `setups::pick` and `launch::plan` word them.
        let none = crate::setups::pick(&[], None).unwrap_err().to_string();
        assert_eq!(code(&none), (409, "no_setups".into()), "{none}");
        let one = [crate::config::Setup { name: "eyzflow on orca".into(), account: "orca".into(), repos: vec!["/nowhere/at/all".into()], ..Default::default() }];
        let unknown = crate::setups::pick(&one, Some("mediaroom")).unwrap_err().to_string();
        assert_eq!(code(&unknown), (404, "no_setup".into()), "{unknown}");
        let world = World { setups: one.to_vec(), ..Default::default() };
        let no_folder = format!("{:#}", crate::launch::plan("Fix the login", None, None, &world).unwrap_err());
        assert_eq!(code(&no_folder), (400, "bad_launch".into()), "{no_folder}");
    }

    #[test]
    fn a_decision_that_is_over_says_how_it_ended() {
        assert!(over(&json!({ "id": "d0123456789ab", "state": "open" })).is_ok() && over(&json!({ "state": "answering" })).is_ok());
        let r = over(&json!({ "id": "d0123456789ab", "state": "dismissed" })).unwrap_err();
        let v: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!((r.status, v["error"]["code"].as_str(), v["error"]["state"].as_str()), (409, Some("closed"), Some("dismissed")));
        let m = merge_error(&json!({ "prs": [{ "key": "o/app#12" }, { "key": "o/app#13" }] }));
        let v: Value = serde_json::from_slice(&m.body).unwrap();
        assert_eq!((m.status, v["error"]["code"].as_str(), &v["error"]["prs"]), (400, Some("merge"), &json!(["o/app#12", "o/app#13"])));
    }

    // ---- over HTTPS, against a tower of the test's own ----

    /// What was asked of the fake tower: (text, space, the phone that hears the reply, source).
    type Asked = (String, Option<String>, Option<String>, String);

    /// A tower of the test's own: real decisions and question sets, a
    /// `spaces.json` in its own folder, a launch that goes when the test
    /// says. Nothing of the user's is read or written, and nothing is typed.
    struct Fake {
        ui: PathBuf,
        shared: Shared,
        decisions: Decisions,
        asks: Asks,
        /// Herdr's spaces the tower keeps nothing about.
        live: Vec<String>,
        world: World,
        sent: Mutex<Vec<(String, String)>>,
        asked: Mutex<Vec<Asked>>,
        level: Mutex<String>,
        model: Mutex<(String, String, Vec<String>)>,
        /// The sessions that hold a space open.
        held: Mutex<Vec<Value>>,
        closed: Mutex<Vec<(String, bool)>>,
        /// A launch opens a session each time the test sends here (none: at once).
        go: Mutex<Option<Receiver<()>>>,
        /// No session of a launch opens.
        fails: AtomicBool,
        /// What the tower opened, kept as the daemon keeps it.
        acts: crate::daemon::acts::Acts,
    }

    /// Hands that end whatever they are asked to, and never touch Herdr.
    struct Easy;
    impl crate::acts::Hands for Easy {
        fn runs(&self, _: &crate::acts::Opened) -> bool {
            true
        }
        fn dirty(&self, _: &crate::worktrees::Place) -> Option<String> {
            None
        }
        fn in_space(&self, _: &str) -> Vec<String> {
            vec!["t1".into()]
        }
        fn shared(&self, _: &crate::worktrees::Place, _: &[String]) -> bool {
            false
        }
        fn forget(&self, _: &crate::acts::Opened) {}
        fn end(&self, _: &str) -> std::result::Result<(), String> {
            Ok(())
        }
        fn remove(&self, _: Option<&str>, _: &crate::worktrees::Place) -> crate::worktrees::Removal {
            crate::worktrees::Removal::Removed
        }
        fn close_space(&self, space: &str) -> std::result::Result<Value, String> {
            Ok(json!({ "closed": space, "sessions_ended": [], "sessions_left": [] }))
        }
    }

    impl Fake {
        fn kept(&self) -> spaces::Spaces {
            spaces::load_from(&self.ui)
        }

        /// The missions into the state, as the daemon's look does.
        fn publish(&self) -> Value {
            let decisions = self.shared.with(|s| s.decisions.clone());
            let threads = Default::default();
            let towers = Default::default();
            let ledger = crate::agenda::Ledger::default();
            let src = crate::missions::Sources {
                sessions: Some(&[]),
                decisions: &decisions,
                ledger: &ledger,
                threads: &threads,
                towers: &towers,
                now: crate::history::now_ms(),
                auto_close_hours: 24,
                active: None,
            };
            let view = crate::missions::view(&self.kept(), &src);
            self.shared.update(|s| s.missions = view.clone());
            view
        }
    }

    impl Tower for Fake {
        fn decisions(&self, space: Option<&str>, all: bool) -> Value {
            self.decisions.refresh();
            self.decisions.listed(space, all)
        }
        fn decision(&self, id: &str) -> Option<Value> {
            self.decisions.view(id)
        }
        fn answer(&self, id: &str, body: &Value, submit: bool) -> Result<Value, Refused> {
            let send = |session: &str, text: &str| -> Result<Value> {
                let mut sent = self.sent.lock().unwrap();
                sent.push((session.to_string(), text.to_string()));
                Ok(json!({ "session": session, "name": session, "queue_id": sent.len(), "sent": true }))
            };
            let agenda = |c: &crate::agenda::Cmd| crate::protocol::Response {
                ok: true,
                error: None,
                state: Some(json!({ "action": c.action, "task": c.target })),
            };
            let reopen = |_: &crate::reopen::Pick| -> Result<Value> { anyhow::bail!("nothing to reopen here") };
            let accounts = |_: bool| -> Result<Value> { anyhow::bail!("no sessions wait for an account here") };
            let plan = |_: &str, _: &str, _: &str| -> Result<Value> { anyhow::bail!("no plan here") };
            self.decisions.answer_with(id, body, submit, &Carry { asks: &self.asks, send: &send, agenda: &agenda, reopen: &reopen, accounts: &accounts, plan: &plan })
        }
        fn dismiss(&self, id: &str) -> Result<Value, Refused> {
            self.decisions.dismiss_with(id, &self.asks)
        }
        fn hold(&self, id: &str, words: &str, unhold: bool, by: &str) -> Result<Value, Refused> {
            self.decisions.hold_with(id, words, unhold, by)
        }
        fn missions(&self) -> Value {
            self.publish()
        }
        fn spaces(&self) -> Vec<String> {
            self.live.iter().cloned().chain(self.remembered()).collect()
        }
        fn remembered(&self) -> Vec<String> {
            self.kept().missions.keys().cloned().collect()
        }
        fn goal(&self, space: &str, goal: Option<&str>, done_when: Option<&str>, standing: Option<bool>) -> Result<Value> {
            let now = crate::history::now_ms();
            Ok(spaces::update_in(&self.ui, |s| {
                let v = crate::missions::goal_by(s, space, goal, done_when, &crate::missions::Change::by("you"), now);
                match standing {
                    Some(on) => crate::missions::standing_in(s, space, on, now),
                    None => v,
                }
            }))
        }
        fn follow(&self, space: &str, on: bool) -> Result<()> {
            spaces::update_in(&self.ui, |s| spaces::set_follow(s, space, on));
            Ok(())
        }
        fn mute(&self, space: &str, on: bool) -> Result<()> {
            spaces::update_in(&self.ui, |s| spaces::set_mute(s, space, on));
            self.decisions.refresh();
            Ok(())
        }
        fn holding(&self, _: &str) -> Result<Vec<Value>> {
            Ok(self.held.lock().unwrap().clone())
        }
        fn close(&self, space: &str, force: bool) -> Result<Value> {
            let held = self.held.lock().unwrap().clone();
            if !held.is_empty() && !force {
                bail!("\"{space}\" isn't closed: a session there is working");
            }
            self.closed.lock().unwrap().push((space.to_string(), force));
            let now = crate::history::now_ms();
            spaces::update_in(&self.ui, |s| {
                let m = s.missions.entry(space.to_string()).or_insert_with(|| Mission::born(now));
                m.state = crate::missions::State::Closed;
                (m.closed_ms, m.archived_ms) = (Some(now), Some(now));
            });
            self.decisions.space_over(space, true);
            self.publish();
            Ok(
                json!({ "closed": space, "sessions_ended": held, "sessions_left": [], "herdr_space": "closed", "worktrees_removed": [], "kept_worktrees": [], "requests_closed": [], "tower_ended": [], "report": "" }),
            )
        }
        fn resolve(&self, space: &str, force: bool) -> Result<Value> {
            let mut v = self.close(space, force)?;
            spaces::update_in(&self.ui, |s| {
                if let Some(m) = s.missions.get_mut(space) {
                    m.achieved_ms = Some(crate::history::now_ms());
                }
            });
            v["resolved"] = true.into();
            Ok(v)
        }
        fn closing(&self, space: &str) -> Result<Value> {
            let held = self.held.lock().unwrap().clone();
            Ok(json!({ "space": space, "ends": held, "busy": held, "stays": [], "worktrees": [], "herdr_space": "closed" }))
        }
        fn setups(&self) -> Result<Value> {
            Ok(
                json!({ "setups": self.world.setups.iter().map(|s| json!({ "name": s.name, "account": s.account, "repos": s.repos })).collect::<Vec<_>>() }),
            )
        }
        fn plan(&self, goal: &str, setup: Option<&str>, name: Option<&str>) -> Result<Plan> {
            crate::launch::plan(goal, setup, name, &self.world)
        }
        fn launch(&self, plan: &Plan, _by: &str, each: &mut dyn FnMut(&Value)) -> Result<Value> {
            let mut started = vec![];
            for (i, _) in plan.sessions.iter().enumerate() {
                if let Some(go) = self.go.lock().unwrap().as_ref() {
                    go.recv_timeout(Duration::from_secs(10))?;
                }
                started.push(match self.fails.load(Ordering::SeqCst) {
                    true => json!({ "error": "its folder is gone" }),
                    false => json!({ "address": format!("t{}", 40 + i), "port": 3000 + i, "task": format!("a1.{}", i + 1) }),
                });
                each(&crate::launch::receipt(plan, &started, Some("a1"), false));
            }
            let now = crate::history::now_ms();
            spaces::update_in(&self.ui, |s| {
                s.agendas.insert(plan.space.clone(), plan.goal.clone());
                s.missions.entry(plan.space.clone()).or_insert_with(|| Mission::born(now));
            });
            self.publish();
            Ok(crate::launch::receipt(plan, &started, Some("a1"), false))
        }
        fn tell(&self, level: Option<&str>, voice: Option<&str>) -> Result<Value> {
            let mut now = self.level.lock().unwrap();
            let mut v = json!({ "level": *now, "voice": self.shared.with(|s| s.tell_voice.clone()) });
            if let Some(to) = voice {
                v["voice_was"] = v["voice"].clone();
                v["voice"] = json!(to);
                self.shared.update(|s| {
                    s.tell_voice = to.to_string();
                    s.muted = to == "off";
                });
            }
            if let Some(l) = level {
                let was = std::mem::replace(&mut *now, l.to_string());
                self.shared.update(|s| s.tell_level = l.to_string());
                v["level"] = json!(l);
                v["was"] = json!(was);
            }
            Ok(v)
        }
        fn model(&self, model: Option<&str>, effort: Option<&str>, restart: bool) -> Result<Value> {
            let mut now = self.model.lock().unwrap();
            if let Some(m) = model {
                now.0 = m.to_string();
            }
            if let Some(e) = effort {
                now.1 = e.to_string();
            }
            let mut cfg = crate::config::Config::default();
            (cfg.brains.model, cfg.brains.effort) = (now.0.clone(), now.1.clone());
            let mut v = crate::brains::report(&cfg, &[]);
            v["restarted"] = json!(if restart { now.2.clone() } else { vec![] });
            self.shared.update(|s| s.brain_model = crate::brains::report(&cfg, &[]));
            Ok(v)
        }
        fn acts(&self) -> Value {
            self.acts.list()
        }
        fn act(&self, id: &str) -> Option<Value> {
            self.acts.view(id)
        }
        fn undo(&self, id: &str, force: bool) -> std::result::Result<Value, crate::acts::Refused> {
            self.acts.undo(Some(id), force, &Easy)
        }
        fn rule(&self, id: &str, answer: Option<&str>) -> std::result::Result<(String, Option<String>), Refused> {
            self.decisions.rule_of(id, answer)
        }
        fn ask(&self, text: &str, phone: Option<String>, source: &str, space: Option<&str>) -> Result<()> {
            self.asked.lock().unwrap().push((text.to_string(), space.map(String::from), phone, source.to_string()));
            Ok(())
        }
    }

    /// The phone API over a fake tower, with a phone paired.
    struct Rig {
        lan: Lan,
        dir: PathBuf,
        addr: SocketAddr,
        fp: String,
        token: String,
        device: String,
        tower: Arc<Fake>,
        history: PathBuf,
        go: Sender<()>,
        /// What the phone's takes are handed to (the transcriber's end).
        stt: Mutex<Receiver<crate::daemon::mic::Utterance>>,
    }

    const SPACES: [&str; 4] = ["Shop", "B2C support", "Docs", "Q&A / ünï"];

    fn rig(name: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("tower-lan-quiet-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (ui, history) = (dir.join("state").join("ui"), dir.join("history.jsonl"));
        std::fs::create_dir_all(&ui).unwrap();
        let now = crate::history::now_ms();
        spaces::update_in(&ui, |s| {
            for n in SPACES {
                s.missions.insert(n.to_string(), Mission::born(now));
            }
        });
        let hub = Arc::new(Hub::default());
        let (stt, takes) = channel();
        let shared = Shared::new(State::new("ptt", "af_bella", "test"));
        let muted = ui.clone();
        let decisions = Decisions::new(shared.clone()).muted_by(move || spaces::load_from(&muted).mute.into_iter().collect());
        let asks = Asks::new(shared.clone(), hub.clone(), "normal").telling_no_brain().with_decisions(decisions.clone());
        // Two folders that are no git checkouts: a session each, no worktrees.
        let repos: Vec<String> = ["eyzflow", "mediaroom"].iter().map(|r| dir.join("work").join(r).to_string_lossy().into_owned()).collect();
        for r in &repos {
            std::fs::create_dir_all(r).unwrap();
        }
        let world = World {
            setups: vec![
                crate::config::Setup { name: "eyzflow + mediaroom on orca".into(), account: "orca".into(), repos, ..Default::default() },
                crate::config::Setup {
                    name: "notes on default".into(),
                    account: "default".into(),
                    repos: vec![dir.join("work").join("eyzflow").to_string_lossy().into_owned()],
                    ..Default::default()
                },
            ],
            spaces: SPACES.iter().map(|s| s.to_string()).collect(),
            open: vec!["Shop".into()],
            accounts: ["default", "orca"]
                .iter()
                .map(|n| crate::accounts::Account { name: n.to_string(), email: format!("{n}@example.invalid") })
                .collect(),
            ..Default::default()
        };
        let (go, gate) = channel();
        let tower = Arc::new(Fake {
            ui,
            shared: shared.clone(),
            decisions,
            asks: asks.clone(),
            live: vec!["Scratch".into()],
            world,
            sent: Mutex::default(),
            asked: Mutex::default(),
            level: Mutex::new("decisions".into()),
            model: Mutex::new((crate::brains::DEFAULT_MODEL.into(), crate::brains::DEFAULT_EFFORT.into(), vec!["tower".into()])),
            held: Mutex::default(),
            closed: Mutex::default(),
            go: Mutex::new(Some(gate)),
            fails: AtomicBool::new(false),
            acts: crate::daemon::acts::Acts::at(dir.join("state").join("acts.json"), shared.clone(), hub.clone()),
        });
        tower.publish();
        let ctx = Ctx { shared, mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() };
        let lan = Lan::with_paths(ctx, dir.join("devices.json"), dir.join("lan")).with_history(history.clone());
        lan.attach(tower.clone());
        let addr = lan
            .start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false, tailscale: "off".into(), ..Default::default() })
            .unwrap();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        let code = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let paired = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code}","name":"Pixel 9"}}"#)).unwrap().1;
        assert_eq!(paired["features"], json!(FEATURES), "a phone knows what this tower has from the moment it pairs");
        let (token, device) = (paired["token"].as_str().unwrap().to_string(), paired["device"]["id"].as_str().unwrap().to_string());
        Rig { lan, dir, addr, fp, token, device, tower, history, go, stt: Mutex::new(takes) }
    }

    impl Rig {
        fn get(&self, path: &str) -> (u16, Value) {
            call(self.addr, &self.fp, "GET", path, Some(&self.token), "").unwrap()
        }
        fn post(&self, path: &str, body: &str) -> (u16, Value) {
            call(self.addr, &self.fp, "POST", path, Some(&self.token), body).unwrap()
        }
        fn delete(&self, path: &str) -> (u16, Value) {
            call(self.addr, &self.fp, "DELETE", path, Some(&self.token), "").unwrap()
        }
        /// A call with no token: refused whatever it is.
        fn stranger(&self, method: &str, path: &str) -> u16 {
            call(self.addr, &self.fp, method, path, None, "{}").unwrap().0
        }
        fn raise(&self, space: &str, text: &str, options: &[&str]) -> String {
            let options = options
                .iter()
                .enumerate()
                .map(|(i, l)| crate::decisions::Opt { label: l.to_string(), pick: i == 0, ..Default::default() })
                .collect();
            self.tower
                .decisions
                .raise(Raise { detail: String::new(), space: Some(space.into()), who: None, why: "design".into(), text: text.into(), options })
                .0
                .id
        }
        fn events(&self) -> Events {
            Events {
                s: open(self.addr, &self.fp, &format!("GET /v1/events HTTP/1.1\r\nAuthorization: Bearer {}\r\n\r\n", self.token)),
                buf: vec![],
            }
        }
        /// An action once it is over.
        fn finished(&self, id: &str) -> Value {
            let until = Instant::now() + Duration::from_secs(10);
            loop {
                let (st, v) = self.get(&format!("/v1/actions/{id}"));
                assert_eq!(st, 200, "{v}");
                if v["action"]["state"] != "running" {
                    return v["action"].clone();
                }
                assert!(Instant::now() < until, "{id} never finished: {v}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        fn end(self) {
            self.lan.stop();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// An open event stream, read an event at a time.
    struct Events {
        s: rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>,
        buf: Vec<u8>,
    }

    impl Events {
        /// The next event called `name` (others before it are passed over): its data.
        fn next(&mut self, name: &str) -> Value {
            loop {
                while let Some(end) = self.buf.windows(2).position(|w| w == b"\n\n") {
                    let block: Vec<u8> = self.buf.drain(..end + 2).collect();
                    let block = String::from_utf8_lossy(&block).into_owned();
                    let line = |k: &str| block.lines().find_map(|l| l.strip_prefix(k).map(String::from));
                    if line("event: ").as_deref() == Some(name) {
                        return serde_json::from_str(&line("data: ").unwrap_or_default()).unwrap_or(Value::Null);
                    }
                }
                let mut b = [0u8; 8192];
                let n = self.s.read(&mut b).unwrap_or_else(|e| panic!("no {name:?} event came: {e}"));
                assert!(n > 0, "the stream ended before a {name:?} event");
                self.buf.extend_from_slice(&b[..n]);
            }
        }

        /// The names of the events that come within `wait`.
        fn names_within(&mut self, wait: Duration) -> Vec<String> {
            let _ = self.s.sock.set_read_timeout(Some(Duration::from_millis(100)));
            let until = Instant::now() + wait;
            let mut b = [0u8; 8192];
            while Instant::now() < until {
                if let Ok(n) = self.s.read(&mut b) {
                    self.buf.extend_from_slice(&b[..n]);
                }
            }
            let _ = self.s.sock.set_read_timeout(Some(Duration::from_secs(10)));
            let text = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
            text.lines().filter_map(|l| l.strip_prefix("event: ").map(String::from)).collect()
        }
    }

    fn ids(v: &Value) -> Vec<String> {
        v["decisions"].as_array().into_iter().flatten().filter_map(|d| d["id"].as_str().map(String::from)).collect()
    }

    fn code(v: &Value) -> Option<&str> {
        v["error"]["code"].as_str()
    }

    /// 14 Nov: a space whose only decision was on hold still showed a 1 by
    /// its name. Held, it needs the user nowhere: not in its space's count
    /// (the missions), the queue's, nor the state's count the tab badge
    /// reads; it is under `on_hold` only. Let go, it is 1 everywhere again.
    #[test]
    fn a_space_whose_only_decision_is_on_hold_needs_nothing() {
        let r = rig("only-held");
        let only = r.raise("Docs", "Publish the guide?", &[]);
        let needs = |r: &Rig| {
            let (_, all) = r.get("/v1/missions");
            all["spaces"].as_array().unwrap().iter().find(|s| s["name"] == "Docs").map(|s| s["needs"].as_u64().unwrap_or(0)).unwrap()
        };
        let counts = |r: &Rig| {
            (needs(r), r.get("/v1/decisions").1["count"].as_u64(), r.get("/v1/decisions?space=Docs").1["count"].as_u64(), r.get("/v1/state").1["decisions_count"].as_u64())
        };
        assert_eq!(counts(&r), (1, Some(1), Some(1), Some(1)));
        assert_eq!(r.post(&format!("/v1/decisions/{only}/hold"), r#"{"reason":"after the launch"}"#).0, 200);
        assert_eq!(counts(&r), (0, Some(0), Some(0), Some(0)), "on hold: nothing needs the user");
        let (_, q) = r.get("/v1/decisions?space=Docs");
        assert_eq!((ids(&q), q["on_hold"][0]["id"].as_str()), (vec![], Some(only.as_str())), "listed under On hold only: {q}");
        assert_eq!(r.post(&format!("/v1/decisions/{only}/unhold"), "{}").0, 200);
        assert_eq!(counts(&r), (1, Some(1), Some(1), Some(1)), "let go: it needs the user again");
    }

    /// The phone holds a decision with the user's words and lets it go:
    /// held, it leaves the queue and the count and is listed under `on_hold`.
    #[test]
    fn a_decision_is_put_on_hold_and_let_go_from_the_phone() {
        let r = rig("hold");
        let ship = r.raise("Shop", "Ship the release?", &[]);
        let other = r.raise("Shop", "Tabs or a sidebar?", &[]);
        assert_eq!(r.get("/v1/decisions").1["count"], 2);
        let (st, v) = r.post(&format!("/v1/decisions/{ship}/hold"), r#"{"reason":"wait for the ingest filters, then we go for prod"}"#);
        assert_eq!((st, v["decision"]["hold"]["reason"].as_str()), (200, Some("wait for the ingest filters, then we go for prod")), "{v}");
        assert!(v["decision"]["hold"]["by"].as_str().is_some_and(|b| b.starts_with("phone:")));
        let (_, q) = r.get("/v1/decisions");
        assert_eq!((q["count"].as_u64(), q["decisions"][0]["id"].as_str(), q["on_hold"][0]["id"].as_str()), (Some(1), Some(other.as_str()), Some(ship.as_str())), "{q}");
        assert_eq!(r.get("/v1/state").1["decisions_count"], 1, "the phone's count");
        assert!(r.get("/v1/state").1.get("decisions_held").is_none(), "the list is the decisions call's, as the queue is");
        // Let go: back in the queue, why said.
        let (st, v) = r.post(&format!("/v1/decisions/{ship}/unhold"), "{}");
        assert_eq!((st, v["decision"]["hold"].is_null(), v["decision"]["back"]["why"].as_str()), (200, true, Some("you let it go from hold")), "{v}");
        assert_eq!(r.get("/v1/decisions").1["count"], 2);
        assert_eq!(code(&r.post(&format!("/v1/decisions/{ship}/unhold"), "{}").1), Some("not_on_hold"));
        assert_eq!(r.post("/v1/decisions/d000000000000/hold", "{}").0, 404);
        r.delete(&format!("/v1/decisions/{other}"));
        let (st, v) = r.post(&format!("/v1/decisions/{other}/hold"), "{}");
        assert_eq!((st, code(&v)), (409, Some("closed")), "{v}");
    }

    #[test]
    fn the_queue_is_read_answered_and_dismissed() {
        let r = rig("queue");
        r.tower.mute("Docs", true).unwrap();
        let tabs = r.raise("Shop", "Tabs or a sidebar?", &["Tabs", "A sidebar"]);
        let delete = r.raise("B2C support", "Delete the old staging database?", &[]);
        let rename = r.raise("Docs", "Rename the guide?", &[]);
        // A session that ended its turn waiting for the user: a question's decision.
        let set = r
            .tower
            .asks
            .waiting("tw-lan-quiet-q1", "mediaroom", None, "PR #42 is ready. Please merge it.", "Merge PR #42", "", false)
            .unwrap();
        let asked = r.tower.decisions.for_question(&set.id)[0].clone();
        // The PRs that wait in one space: a merge.
        let pr = crate::prs::Pr {
            repo: "o/app".into(),
            number: 12,
            base: "dev".into(),
            state: "open".into(),
            mergeable: "mergeable".into(),
            checks: crate::prs::Checks { state: "passing".into(), ..Default::default() },
            session: "tw-lan-quiet-s2".into(),
            name: "api".into(),
            space: Some("Shop".into()),
            waiting: true,
            found_ms: crate::history::now_ms(),
            ..Default::default()
        };
        r.tower.decisions.sync_prs(std::slice::from_ref(&pr));
        let merge = r.tower.decisions.for_prs(std::slice::from_ref(&pr))[0].clone();

        // Nothing without a token.
        for (m, p) in [
            ("GET", "/v1/decisions"),
            ("GET", "/v1/decisions/d0123456789ab"),
            ("POST", "/v1/decisions/d0123456789ab/answer"),
            ("DELETE", "/v1/decisions/d0123456789ab"),
        ] {
            assert_eq!(r.stranger(m, p), 401, "{m} {p}");
        }
        // The main queue: every open decision outside muted spaces, in the daemon's order.
        let (st, main) = r.get("/v1/decisions");
        assert_eq!((st, main["count"].as_u64(), &main["space"]), (200, Some(4), &Value::Null), "{main}");
        assert_eq!(
            ids(&main),
            ids(&json!({ "decisions": r.tower.shared.get().decisions.into_iter().filter(|d| d["muted"] == false).collect::<Vec<_>>() }))
        );
        assert!(!ids(&main).contains(&rename), "a muted space's decision is not in the main queue");
        let d = main["decisions"].as_array().unwrap().iter().find(|d| d["id"] == tabs.as_str()).unwrap();
        assert_eq!(
            (d["kind"].as_str(), d["space"].as_str(), d["state"].as_str(), d["pick"].as_str(), d["muted"].as_bool(), d["count"].as_u64()),
            (Some("choice"), Some("Shop"), Some("open"), Some("Tabs"), Some(false), Some(1))
        );
        assert_eq!(d["options"], json!([{ "label": "Tabs", "why": "", "pick": true }, { "label": "A sidebar", "why": "", "pick": false }]));
        for k in ["key", "sessions", "waiting", "title", "why", "prs", "question_id", "task", "first_ms", "last_ms"] {
            assert!(d.get(k).is_some(), "a decision has {k}: {d}");
        }
        // A space's own, muted or not, by its whole name in any case; never another one's that starts alike.
        for path in ["/v1/decisions?space=B2C%20support", "/v1/decisions?space=B2C+support", "/v1/decisions?space=b2c%20SUPPORT"] {
            let (st, own) = r.get(path);
            assert_eq!((st, ids(&own), own["space"].as_str()), (200, vec![delete.clone()], Some("B2C support")), "{path}");
        }
        let (_, docs) = r.get("/v1/decisions?space=Docs");
        assert_eq!((ids(&docs), docs["decisions"][0]["muted"].as_bool()), (vec![rename.clone()], Some(true)));
        assert_eq!(r.get("/v1/decisions?space=Sho").1["count"], 0, "a part of a name is no space");
        assert_eq!(r.get("/v1/decisions?space=Nowhere").1["count"], 0);
        // One, by its id.
        let (st, one) = r.get(&format!("/v1/decisions/{merge}"));
        assert_eq!(
            (st, one["decision"]["kind"].as_str(), one["decision"]["prs"][0]["key"].as_str()),
            (200, Some("merge"), Some("o/app#12")),
            "{one}"
        );
        assert_eq!(one["decision"]["prs"][0]["merge"]["can"], true, "what its PR's own calls need is there");
        for id in ["d000000000000", "nope", "q0123456789ab"] {
            let (st, v) = r.get(&format!("/v1/decisions/{id}"));
            assert_eq!((st, code(&v)), (404, Some("no_decision")), "{id}");
        }

        // An answer that doesn't fit changes nothing.
        let answer = |id: &str, body: &str| r.post(&format!("/v1/decisions/{id}/answer"), body);
        for (body, want) in
            [(r#"{"choice":7}"#, "bad_answer"), ("{}", "bad_answer"), ("not json", "bad_json"), (r#"{"other":"   "}"#, "bad_answer")]
        {
            let (st, v) = answer(&tabs, body);
            assert_eq!((st, code(&v)), (400, Some(want)), "{body}: {v}");
        }
        assert_eq!(answer("d000000000000", r#"{"choice":0}"#).0, 404);
        // An option by its number: answered, and gone from the queue.
        let (st, v) = answer(&tabs, r#"{"choice":1}"#);
        assert_eq!(
            (st, v["state"].as_str(), v["answer"].as_str(), v["id"].as_str()),
            (200, Some("answered"), Some("A sidebar"), Some(tabs.as_str())),
            "{v}"
        );
        assert!(!ids(&r.get("/v1/decisions").1).contains(&tabs));
        // Answered already: a conflict that says how it ended, not a failure.
        let (st, v) = answer(&tabs, r#"{"choice":0}"#);
        assert_eq!((st, code(&v), v["error"]["state"].as_str()), (409, Some("closed"), Some("answered")), "{v}");
        assert_eq!(r.get(&format!("/v1/decisions/{tabs}")).1["decision"]["state"], "answered", "one that closed lately can still be read");
        // The user's own words, with what a terminal would obey taken out.
        let (st, v) = answer(&delete, r#"{"other":"keep it\u001b[2J a week"}"#);
        assert_eq!((st, v["state"].as_str(), v["answer"].as_str()), (200, Some("answered"), Some("keep it[2J a week")), "{v}");

        // A merge is its PR's own flow.
        let (st, v) = answer(&merge, r#"{"choice":0}"#);
        assert_eq!((st, code(&v), &v["error"]["prs"]), (400, Some("merge"), &json!(["o/app#12"])), "{v}");
        assert_eq!(r.post(&format!("/v1/decisions/{merge}/interpret"), r#"{"text":"the first"}"#).0, 400);
        // Dismissed: out of the queue; again says the same; an unknown one is a 404.
        let (st, v) = r.delete(&format!("/v1/decisions/{merge}"));
        assert_eq!((st, v["dismissed"].as_str(), v["state"].as_str()), (200, Some(merge.as_str()), Some("dismissed")), "{v}");
        assert_eq!(r.delete(&format!("/v1/decisions/{merge}")).1["state"], "dismissed");
        assert_eq!(r.delete("/v1/decisions/d000000000000").0, 404);
        let (st, v) = answer(&merge, r#"{"other":"later"}"#);
        assert_eq!((st, code(&v), v["error"]["state"].as_str()), (409, Some("closed"), Some("dismissed")));

        // A question's decision takes what its question set takes; its spoken answer is read against its tab.
        let (st, v) = r.post(&format!("/v1/decisions/{asked}/interpret"), r#"{"tab":0,"text":"yes, merged"}"#);
        assert_eq!((st, v["choice"]["other"].as_str(), v.get("answer")), (200, Some("yes, merged"), None), "{v}");
        assert_eq!(r.post(&format!("/v1/decisions/{asked}/interpret"), r#"{"tab":3,"text":"x"}"#).0, 400);
        assert_eq!(r.post(&format!("/v1/decisions/{asked}/voice?tab=0"), "not audio").0, 415);
        assert_eq!(r.post("/v1/decisions/d000000000000/voice", "not audio").0, 404);
        // Files go with a question's typed answers, and are checked before anything is typed or saved.
        let form = |id: &str, fields: &[(&str, &str)], files: &[(&str, &str, &str, &[u8])]| {
            let body = multipart::build("XB", fields, files);
            call_raw(r.addr, &r.fp, "POST", &format!("/v1/decisions/{id}/answer"), &r.token, "multipart/form-data; boundary=XB", &body)
                .unwrap()
        };
        let png: &[u8] = b"\x89PNG\r\n\x1a\n....";
        let (st, v) = form(&asked, &[], &[("tab0", "a.png", "image/png", png)]);
        assert_eq!((st, code(&v)), (400, Some("no_json")));
        let (st, v) = form(&asked, &[("json", r#"{"other":"see the picture"}"#)], &[("tab0", "a.png", "image/png", b"not an image")]);
        assert_eq!((st, code(&v)), (415, Some("not_an_image")), "{v}");
        let (st, v) =
            form(&asked, &[("json", r#"{"answers":[{"tab":4,"choice":{"other":"x"}}]}"#)], &[("tab4", "a.png", "image/png", png)]);
        assert_eq!((st, code(&v)), (400, Some("bad_tab")));
        let late = r.raise("Shop", "Ship it on Friday?", &[]);
        let (st, v) = form(&late, &[("json", r#"{"other":"yes"}"#)], &[("tab0", "a.png", "image/png", png)]);
        assert_eq!((st, code(&v)), (400, Some("no_files")), "{v}");
        // A spoken answer for one that is no question: its options are the one tab, and `answer` is what to send.
        let pick = r.raise("Shop", "Blue or green?", &["Blue", "Green"]);
        let (st, v) = r.post(&format!("/v1/decisions/{pick}/interpret"), r#"{"text":"the second one"}"#);
        assert_eq!((st, &v["answer"], &v["choice"]["options"]), (200, &json!({ "choice": 1 }), &json!([1])), "{v}");
        // Its session is gone when the answer comes: the question went with it, and so did the decision.
        let (st, v) = answer(&asked, r#"{"other":"merged"}"#);
        assert_eq!((st, code(&v), v["error"]["state"].as_str()), (409, Some("closed"), Some("stopped")), "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("isn't there any more"), "{v}");
        assert_eq!(r.post(&format!("/v1/decisions/{asked}/interpret"), r#"{"text":"x"}"#).0, 409);
        // Nothing was sent anywhere: no session waited on what was answered.
        assert!(r.tower.sent.lock().unwrap().is_empty());
        r.end();
    }

    #[test]
    fn what_the_tower_opened_is_shown_and_undone() {
        let r = rig("acts");
        for (m, p) in [("GET", "/v1/acts"), ("GET", "/v1/acts/r0123456789ab"), ("POST", "/v1/acts/r0123456789ab/undo")] {
            assert_eq!(r.stranger(m, p), 401, "{m} {p}");
        }
        assert_eq!(r.get("/v1/acts"), (200, json!({ "acts": [] })));
        let mut ev = r.events();
        ev.next("missions");
        // The tower opens a session for a request: an act, with what it assumed.
        let what = json!({
            "kind": "start", "space": "Shop",
            "sessions": [{ "address": "t1", "name": "shop-web", "first": "look at the cart" }],
            "assumed": { "account": "orca", "account_why": "the default one: none was named", "folder": "/w/shop", "folder_why": "as asked" },
        });
        let kept = r.tower.acts.record(&what, None).unwrap();
        let id = kept["id"].as_str().unwrap().to_string();
        let seen = ev.next("act");
        assert_eq!((seen["id"].as_str(), seen["line"].as_str(), seen["undo"]["possible"].as_bool()), (Some(id.as_str()), Some("Opened web in Shop."), Some(true)), "{seen}");
        let (st, v) = r.get("/v1/acts");
        assert_eq!((st, v["acts"].as_array().map(Vec::len), &v["acts"][0]), (200, Some(1), &kept));
        let (st, v) = r.get(&format!("/v1/acts/{id}"));
        assert_eq!((st, &v["act"]["assumed"]), (200, &json!([{ "what": "account", "value": "orca", "why": "the default one: none was named" }, { "what": "folder", "value": "shop", "why": "as asked" }])));
        assert_eq!(r.get("/v1/state").1.get("acts"), None, "the phone's state leaves the list out");
        // Its session was given more work: the undo is refused, and says who and why.
        r.tower.acts.given("t1", "also fix the footer");
        assert_eq!(ev.next("act")["undo"], json!({ "possible": false, "why": "shop-web was given more work since (\"also fix the footer\")" }));
        let (st, v) = r.post(&format!("/v1/acts/{id}/undo"), "{}");
        assert_eq!((st, code(&v)), (409, Some("held")), "{v}");
        assert_eq!(v["error"]["sessions"], json!([{ "address": "t1", "name": "shop-web", "why": "was given more work since (\"also fix the footer\")" }]));
        assert!(v["error"]["message"].as_str().unwrap().ends_with("Send {\"force\": true} to undo it anyway."), "{v}");
        assert_eq!(r.get(&format!("/v1/acts/{id}")).1["act"]["state"], "done", "a refusal changes nothing");
        // Forced, it goes: its session is ended, and the act says how it closed.
        let (st, v) = r.post(&format!("/v1/acts/{id}/undo"), r#"{"force":true}"#);
        assert_eq!(st, 200, "{v}");
        assert_eq!((v["closing"].as_str(), v["act"]["state"].as_str(), v["act"]["undo"]["possible"].as_bool()), (Some("Closed it. Nothing else changed."), Some("undone"), Some(false)), "{v}");
        assert_eq!(v["sessions_ended"], json!(["t1"]));
        assert_eq!(ev.next("act")["state"], "undone");
        // Once: a second time it says so; an id nobody has is a 404.
        let (st, v) = r.post(&format!("/v1/acts/{id}/undo"), "{}");
        assert_eq!((st, code(&v), v["error"]["state"].as_str()), (409, Some("undone"), Some("undone")), "{v}");
        for path in ["/v1/acts/r000000000000", "/v1/acts/nope", "/v1/acts/d0123456789ab"] {
            assert_eq!(code(&r.get(path).1), Some("no_act"), "{path}");
            let (st, v) = r.post(&format!("{path}/undo"), "{}");
            assert_eq!((st, code(&v)), (404, Some("no_act")), "{path}");
        }
        r.end();
    }

    #[test]
    fn an_answer_is_made_a_rule_where_that_is_offered() {
        let r = rig("rules");
        let d = &r.tower.decisions;
        let choice = r.raise("Shop", "Tabs or a sidebar for settings?", &["Tabs", "A sidebar"]);
        let risky = r.raise("Shop", "Deploy the cart change to production?", &["Yes", "No"]);
        let plain = d.raise(Raise { detail: String::new(), space: Some("Shop".into()), who: None, why: "waiting".into(), text: "The APK needs testing".into(), options: vec![] }).0.id;
        assert_eq!(r.stranger("POST", &format!("/v1/decisions/{choice}/rule")), 401);
        // Not answered yet: nothing to make a rule of.
        let (st, v) = r.post(&format!("/v1/decisions/{choice}/rule"), "{}");
        assert_eq!((st, code(&v)), (409, Some("not_answered")), "{v}");
        // The answer says whether a rule can be made, and what would be asked.
        let (st, v) = r.post(&format!("/v1/decisions/{choice}/answer"), r#"{"choice":1}"#);
        let text = "Make this a rule: when a session in Shop asks \"Tabs or a sidebar for settings?\", answer \"A sidebar\" without asking me.";
        assert_eq!((st, &v["rule"]), (200, &json!({ "can": true, "text": text })), "{v}");
        // One tap: asked of the tower in that space's view, as a typed request is.
        let (st, v) = r.post(&format!("/v1/decisions/{choice}/rule"), "{}");
        assert_eq!((st, v), (202, json!({ "sent": text, "space": "Shop" })));
        let asked = r.tower.asked.lock().unwrap().last().cloned().unwrap();
        assert_eq!((asked.0.as_str(), asked.1.as_deref(), asked.3.starts_with("phone:")), (text, Some("Shop"), true));
        // In the app's own words for the answer.
        let (_, v) = r.post(&format!("/v1/decisions/{choice}/rule"), r#"{"answer":"the sidebar, always"}"#);
        assert!(v["sent"].as_str().unwrap().contains("answer \"the sidebar, always\""), "{v}");
        // Never for what the user always decides, nor for what is no question or choice.
        let (st, v) = r.post(&format!("/v1/decisions/{risky}/answer"), r#"{"choice":0}"#);
        assert_eq!((st, &v["rule"]), (200, &json!({ "can": false, "text": null })), "{v}");
        let (st, v) = r.post(&format!("/v1/decisions/{risky}/rule"), "{}");
        assert_eq!((st, code(&v)), (409, Some("no_rule")), "{v}");
        r.post(&format!("/v1/decisions/{plain}/answer"), r#"{"other":"I tested it, go on"}"#);
        assert_eq!(code(&r.post(&format!("/v1/decisions/{plain}/rule"), "{}").1), Some("no_rule"));
        assert_eq!(code(&r.post("/v1/decisions/d000000000000/rule", "{}").1), Some("no_decision"));
        assert_eq!(r.tower.asked.lock().unwrap().len(), 2, "only what was offered was asked");
        r.end();
    }

    #[test]
    fn an_answer_goes_to_the_sessions_that_wait() {
        let r = rig("sends");
        let who = |a: &str| Some(crate::decisions::Who::new(a, &format!("s-{a}")));
        let options = vec![
            crate::decisions::Opt { label: "Tabs".into(), pick: true, text: "Use tabs, as in the mock.".into(), ..Default::default() },
            crate::decisions::Opt { label: "A sidebar".into(), ..Default::default() },
        ];
        let d = &r.tower.decisions;
        let id = d
            .raise(Raise {
                detail: String::new(),
                space: Some("Shop".into()),
                who: who("tw-q-a"),
                why: "design".into(),
                text: "Tabs or a sidebar?".into(),
                options,
            })
            .0
            .id;
        d.raise(Raise {
            detail: String::new(),
            space: Some("Shop".into()),
            who: who("tw-q-b"),
            why: "design".into(),
            text: "tabs or a sidebar".into(),
            options: vec![],
        });
        let (st, v) = r.post(&format!("/v1/decisions/{id}/answer"), r#"{"choice":0}"#);
        assert_eq!((st, v["state"].as_str(), v["sent"].as_array().map(Vec::len)), (200, Some("answered"), Some(2)), "{v}");
        assert_eq!(
            *r.tower.sent.lock().unwrap(),
            [
                ("tw-q-a".to_string(), "Use tabs, as in the mock.".to_string()),
                ("tw-q-b".to_string(), "Use tabs, as in the mock.".to_string())
            ]
        );
        // A task that failed twice: sent again, or dropped.
        let failed = |task: &str| {
            d.failed(crate::decisions::Failure {
                task: task.into(),
                title: "fix the login".into(),
                space: Some("Shop".into()),
                who: who("tw-q-a"),
                why: "the tests fail".into(),
            })
            .0
            .id
        };
        let (again, drop) = (failed("a3.1"), failed("a3.2"));
        let (st, v) = r.post(&format!("/v1/decisions/{again}/answer"), r#"{"choice":0}"#);
        assert_eq!(
            (st, v["state"].as_str(), v["task"].as_str(), v["requeued"]["action"].as_str()),
            (200, Some("answered"), Some("a3.1"), Some("requeue")),
            "{v}"
        );
        let (st, v) = r.post(&format!("/v1/decisions/{drop}/answer"), r#"{"choice":1}"#);
        assert_eq!((st, v["abandoned"]["action"].as_str()), (200, Some("abandon")), "{v}");
        assert_eq!(r.get("/v1/decisions").1["count"], 0);
        assert_eq!(
            r.get("/v1/decisions?space=Shop&all=1").1["closed"].as_array().map(Vec::len),
            Some(3),
            "the last that closed, when asked for"
        );
        r.end();
    }

    #[test]
    fn a_new_decision_is_a_new_id_and_a_repeat_is_not() {
        let r = rig("events");
        let before = r.raise("Shop", "Tabs or a sidebar?", &[]);
        let mut ev = r.events();
        // A stream starts with what there is: who it is, the state, the queue, the missions.
        let hello = ev.next("hello");
        assert_eq!(hello["features"], json!(FEATURES));
        let state = ev.next("state");
        assert_eq!((state["decisions_count"].as_u64(), state["tell_level"].as_str()), (Some(1), Some("decisions")));
        for big in ["decisions", "missions", "threads"] {
            assert!(state.get(big).is_none(), "the state carries no {big}: each has its own call and event");
        }
        let first = ev.next("decisions");
        assert_eq!((ids(&first), first["count"].as_u64()), (vec![before.clone()], Some(1)));
        let missions = ev.next("missions");
        assert_eq!(missions["spaces"].as_array().map(Vec::len), Some(SPACES.len()));

        // A new decision: an id the phone has not seen.
        let ship = r.raise("B2C support", "Ship it on Friday?", &["Yes", "No"]);
        let e = ev.next("decisions");
        assert_eq!(e["count"], 2);
        let new: Vec<String> = ids(&e).into_iter().filter(|id| !ids(&first).contains(id)).collect();
        assert_eq!(new, [ship.clone()], "the one to notify for");
        let was = e["decisions"].as_array().unwrap().iter().find(|d| d["id"] == ship.as_str()).unwrap().clone();
        assert_eq!((was["count"].as_u64(), was["state"].as_str(), was["muted"].as_bool()), (Some(1), Some("open"), Some(false)));
        // The same thing again (another session, the same line): the same id; `count` and `last_ms` move.
        std::thread::sleep(Duration::from_millis(5));
        let d = &r.tower.decisions;
        let again = d
            .raise(Raise {
                detail: String::new(),
                space: Some("B2C support".into()),
                who: Some(crate::decisions::Who::new("tw-q-c", "api")),
                why: "deploy".into(),
                text: "ship it on friday".into(),
                options: vec![],
            })
            .0;
        assert_eq!((again.id.as_str(), again.fresh), (ship.as_str(), false));
        let e2 = ev.next("decisions");
        assert_eq!(ids(&e2).iter().filter(|id| !ids(&e).contains(id)).count(), 0, "no new id: nothing to notify for");
        let now = e2["decisions"].as_array().unwrap().iter().find(|d| d["id"] == ship.as_str()).unwrap();
        assert_eq!(
            (now["count"].as_u64(), now["first_ms"].as_u64(), now["key"].as_str()),
            (Some(2), was["first_ms"].as_u64(), was["key"].as_str())
        );
        assert!(now["last_ms"].as_u64() > was["last_ms"].as_u64(), "{now}");
        assert_eq!(now["waiting"], 1);
        // In a muted space: listed and marked, and not counted.
        r.tower.mute("Docs", true).unwrap();
        let quiet = r.raise("Docs", "Rename the guide?", &[]);
        let e3 = ev.next("decisions");
        let muted = e3["decisions"].as_array().unwrap().iter().find(|d| d["id"] == quiet.as_str()).unwrap();
        assert_eq!((muted["muted"].as_bool(), e3["count"].as_u64(), ids(&e3).len()), (Some(true), Some(2), 3));
        // Answered from the phone: gone from the next one, and the count with it.
        assert_eq!(r.post(&format!("/v1/decisions/{ship}/answer"), r#"{"choice":0}"#).0, 200);
        let e4 = ev.next("decisions");
        assert_eq!((ids(&e4).contains(&ship), e4["count"].as_u64()), (false, Some(1)));
        // The state said only its count each time: a small event.
        assert_eq!(r.get("/v1/state").1["decisions_count"], 1);

        // A space's switch shows in the missions event.
        assert_eq!(r.post("/v1/spaces/Shop/follow", r#"{"on":true}"#).0, 200);
        let m = ev.next("missions");
        let shop = m["spaces"].as_array().unwrap().iter().find(|s| s["name"] == "Shop").unwrap();
        assert_eq!((shop["follow"].as_bool(), shop["needs"].as_u64()), (Some(true), Some(1)));

        // A message between two sessions: the history's entry, and its space's thread.
        let peer = json!({ "at": 1794158000000u64, "kind": "peer", "session": "t6", "name": "api", "from": "t5", "from_name": "web", "space": "Shop", "text": "Which port does the api use?", "status": "delivered", "record": true });
        std::fs::write(&r.history, format!("{}\n{peer}\n", json!({ "at": 1, "kind": "note", "text": "before the stream" }))).unwrap();
        let t = ev.next("thread");
        assert_eq!(
            (t["space"].as_str(), t["message"]["id"].as_str(), t["message"]["text"].as_str()),
            (Some("Shop"), Some("h2"), Some("Which port does the api use?")),
            "{t}"
        );
        assert_eq!(
            (t["message"]["from"].as_str(), t["message"]["to_name"].as_str(), t["message"]["delivered"].as_bool()),
            (Some("t5"), Some("api"), Some(true))
        );
        // What moved nothing a phone shows sends no `state`: here, only a ping at most.
        ev.names_within(Duration::from_millis(600));
        r.tower.shared.update(|s| s.threads.entry("Shop".into()).or_default().push(json!({ "text": "x" })));
        let quiet_names = ev.names_within(Duration::from_millis(900));
        assert!(!quiet_names.iter().any(|n| n == "state" || n == "decisions" || n == "missions"), "{quiet_names:?}");
        r.end();
    }

    #[test]
    fn a_space_has_a_goal_its_switches_and_an_end() {
        let r = rig("spaces");
        for (m, p) in [
            ("GET", "/v1/missions"),
            ("POST", "/v1/spaces/Shop/goal"),
            ("POST", "/v1/spaces/Shop/follow"),
            ("POST", "/v1/spaces/Shop/mute"),
            ("POST", "/v1/spaces/Shop/close"),
        ] {
            assert_eq!(r.stranger(m, p), 401, "{m} {p}");
        }
        let (st, all) = r.get("/v1/missions");
        assert_eq!(
            (st, all["spaces"].as_array().map(Vec::len), &all["quiet"], &all["closed_today"]),
            (200, Some(4), &json!([]), &json!([])),
            "{all}"
        );
        let row = &all["spaces"][0];
        for k in [
            "name",
            "goal",
            "done_when",
            "standing",
            "state",
            "follow",
            "mute",
            "needs",
            "report",
            "sessions",
            "requests",
            "thread_count",
            "tower",
            "quiet_since_ms",
            "ready_since_ms",
            "auto_close_ms",
        ] {
            assert!(row.get(k).is_some(), "a mission has {k}: {row}");
        }

        // A goal, what counts as done, and whether the space is kept: each when given.
        let (st, v) = r.post(
            "/v1/spaces/B2C%20support/goal",
            r#"{"goal":"Answer the  support tickets","done_when":"The inbox is empty","standing":false}"#,
        );
        assert_eq!(
            (st, v["space"].as_str(), v["goal"].as_str(), v["done_when"].as_str(), v["standing"].as_bool()),
            (200, Some("B2C support"), Some("Answer the support tickets"), Some("The inbox is empty"), Some(false)),
            "{v}"
        );
        assert_eq!(
            (v["mission"]["name"].as_str(), v["mission"]["goal"].as_str(), v["state"].as_str()),
            (Some("B2C support"), Some("Answer the support tickets"), Some("open"))
        );
        let (st, v) = r.post("/v1/spaces/b2c%20SUPPORT/goal", r#"{"standing":true}"#);
        assert_eq!(
            (st, v["goal"].as_str(), v["done_when"].as_str(), v["mission"]["standing"].as_bool()),
            (200, Some("Answer the support tickets"), Some("The inbox is empty"), Some(true)),
            "what wasn't sent stays: {v}"
        );
        let (st, v) = r.post("/v1/spaces/B2C%20support/goal", r#"{"goal":""}"#);
        assert_eq!(
            (st, v["goal"].as_str(), v["done_when"].as_str()),
            (200, Some(""), Some("The inbox is empty")),
            "an empty goal clears it: {v}"
        );
        // A name is decoded whole: a slash, an ampersand, letters outside ASCII.
        let (st, v) = r.post("/v1/spaces/Q%26A%20%2F%20%C3%BCn%C3%AF/goal", r#"{"goal":"Write the FAQ"}"#);
        assert_eq!((st, v["space"].as_str(), v["mission"]["goal"].as_str()), (200, Some("Q&A / ünï"), Some("Write the FAQ")), "{v}");
        assert_eq!(r.tower.kept().agendas.get("Q&A / ünï").map(String::as_str), Some("Write the FAQ"));
        // A space Herdr has and the tower keeps nothing about yet can get one too.
        assert_eq!(r.post("/v1/spaces/Scratch/goal", r#"{"goal":"Try things"}"#).1["mission"]["goal"], "Try things");
        // A plus in a path is a plus, a part of a name is no space, and neither is nothing.
        for path in ["/v1/spaces/B2C+support/goal", "/v1/spaces/B2C/goal", "/v1/spaces/Nowhere/goal", "/v1/spaces/%20/goal"] {
            let (st, v) = r.post(path, r#"{"goal":"x"}"#);
            assert_eq!((st, code(&v)), (404, Some("no_space")), "{path}");
        }
        for body in ["{}", r#"{"goal":7}"#, r#"{"standing":"yes"}"#] {
            let (st, v) = r.post("/v1/spaces/Shop/goal", body);
            assert_eq!((st, code(&v)), (400, Some("bad_goal")), "{body}");
        }
        assert_eq!(code(&r.post("/v1/spaces/Shop/goal", "not json").1), Some("bad_json"));

        // Follow and mute: kept with the spaces, and the mission says so at once.
        let (st, v) = r.post("/v1/spaces/Shop/follow", r#"{"on":true}"#);
        assert_eq!(
            (st, v["space"].as_str(), v["follow"].as_bool(), v["mission"]["follow"].as_bool()),
            (200, Some("Shop"), Some(true), Some(true)),
            "{v}"
        );
        assert!(r.tower.kept().follow.contains("Shop"));
        assert_eq!(r.post("/v1/spaces/shop/follow", r#"{"on":false}"#).1["mission"]["follow"], false);
        assert!(r.tower.kept().follow.is_empty());
        let needs = r.raise("Shop", "Tabs or a sidebar?", &[]);
        assert_eq!(r.get("/v1/state").1["decisions_count"], 1);
        let (st, v) = r.post("/v1/spaces/Shop/mute", r#"{"on":true}"#);
        assert_eq!(
            (st, v["mute"].as_bool(), v["mission"]["mute"].as_bool(), v["mission"]["needs"].as_u64()),
            (200, Some(true), Some(true), Some(1)),
            "{v}"
        );
        assert_eq!(r.get("/v1/state").1["decisions_count"], 0, "a muted space's decisions leave the count at once");
        assert_eq!((r.get("/v1/decisions").1["count"].as_u64(), ids(&r.get("/v1/decisions?space=Shop").1)), (Some(0), vec![needs.clone()]));
        assert_eq!(r.post("/v1/spaces/Shop/mute", r#"{"on":false}"#).1["mute"], false);
        assert_eq!(r.get("/v1/decisions").1["count"], 1);
        for body in ["{}", r#"{"on":"yes"}"#, "nope"] {
            assert_eq!(code(&r.post("/v1/spaces/Shop/mute", body).1), Some("bad_on"), "{body}");
        }
        assert_eq!(r.post("/v1/spaces/Nowhere/follow", r#"{"on":true}"#).0, 404);

        // Closing: refused while a session there works, naming it; nothing is started.
        *r.tower.held.lock().unwrap() = vec![json!({ "address": "t7", "name": "shop-api", "status": "working" })];
        let (st, v) = r.post("/v1/spaces/Shop/close", "{}");
        assert_eq!((st, code(&v), &v["error"]["sessions"][0]["address"]), (409, Some("busy"), &json!("t7")), "{v}");
        assert!(v["error"]["message"].as_str().unwrap().contains("shop-api (t7) is working"), "{v}");
        assert!(r.get("/v1/actions").1["actions"].as_array().unwrap().is_empty() && r.tower.closed.lock().unwrap().is_empty());
        // Forced: answered at once with the action; its receipt comes when it is done.
        let (st, v) = r.post("/v1/spaces/shop/close", r#"{"force":true}"#);
        assert_eq!((st, v["action"]["kind"].as_str(), v["action"]["space"].as_str()), (202, Some("close"), Some("Shop")), "{v}");
        assert_eq!(v["action"]["by"], format!("phone:{}", r.device));
        let done = r.finished(v["action"]["id"].as_str().unwrap());
        assert_eq!(
            (done["state"].as_str(), done["receipt"]["closed"].as_str(), &done["error"]),
            (Some("done"), Some("Shop"), &Value::Null),
            "{done}"
        );
        assert!(done["finished_ms"].as_u64() >= done["started_ms"].as_u64());
        assert_eq!(*r.tower.closed.lock().unwrap(), [("Shop".to_string(), true)]);
        // Closed: out of the missions, named in today's closed, and its decisions went with it.
        let (_, all) = r.get("/v1/missions");
        assert!(!all["spaces"].as_array().unwrap().iter().any(|s| s["name"] == "Shop") && all["closed_today"] == json!(["Shop"]), "{all}");
        assert_eq!(r.get(&format!("/v1/decisions/{needs}")).1["decision"]["state"], "stopped");
        assert_eq!(r.post("/v1/spaces/Nowhere/close", "{}").0, 404);
        // One that fails says why, where the app can read it.
        *r.tower.held.lock().unwrap() = vec![];
        let (_, v) = r.post("/v1/spaces/Docs/close", "{}");
        assert_eq!(r.finished(v["action"]["id"].as_str().unwrap())["state"], "done");

        // Resolving: only a space with a goal; what it would end, first; then the same close, its goal achieved.
        let (st, v) = r.post("/v1/spaces/B2C%20support/resolve", "{}");
        assert_eq!((st, code(&v)), (409, Some("no_goal")), "{v}");
        assert_eq!(r.post("/v1/spaces/B2C%20support/goal", r#"{"goal":"Answer every support ticket"}"#).0, 200);
        *r.tower.held.lock().unwrap() = vec![json!({ "address": "t8", "name": "b2c-api", "status": "blocked" })];
        let (st, v) = r.get("/v1/spaces/B2C%20support/closing");
        assert_eq!((st, v["space"].as_str(), v["busy"][0]["address"].as_str()), (200, Some("B2C support"), Some("t8")), "{v}");
        let (st, v) = r.post("/v1/spaces/B2C%20support/resolve", "{}");
        assert_eq!((st, code(&v)), (409, Some("busy")), "{v}");
        let (st, v) = r.post("/v1/spaces/B2C%20support/resolve", r#"{"force":true}"#);
        assert_eq!((st, v["action"]["kind"].as_str()), (202, Some("resolve")), "{v}");
        let done = r.finished(v["action"]["id"].as_str().unwrap());
        assert_eq!((done["state"].as_str(), done["receipt"]["resolved"].as_bool()), (Some("done"), Some(true)), "{done}");
        assert!(spaces::load_from(&r.tower.ui).missions["B2C support"].achieved_ms.is_some());
        assert_eq!(r.get("/v1/spaces/Nowhere/closing").0, 404);
        r.end();
    }

    #[test]
    fn a_launch_answers_at_once_and_says_each_step() {
        let r = rig("launch");
        assert_eq!((r.stranger("GET", "/v1/setups"), r.stranger("POST", "/v1/launch"), r.stranger("GET", "/v1/actions")), (401, 401, 401));
        let (st, v) = r.get("/v1/setups");
        assert_eq!(
            (st, v["setups"][0]["name"].as_str(), v["setups"][0]["account"].as_str(), v["setups"][0]["repos"].as_array().map(Vec::len)),
            (200, Some("eyzflow + mediaroom on orca"), Some("orca"), Some(2)),
            "{v}"
        );
        // What it would do, without doing any of it.
        let (st, plan) = r.post("/v1/launch", r#"{"goal":"Fix the login redirect","dry_run":true}"#);
        assert_eq!(
            (st, plan["space"].as_str(), plan["dry_run"].as_bool(), plan["made"].as_bool(), plan["sessions"].as_array().map(Vec::len)),
            (200, Some("fix-login"), Some(true), Some(true), Some(2)),
            "{plan}"
        );
        assert_eq!(
            (
                plan["assumed"]["setup"].as_str(),
                plan["assumed"]["account"].as_str(),
                plan["sessions"][0]["name"].as_str(),
                &plan["sessions"][0]["address"]
            ),
            (Some("eyzflow + mediaroom on orca"), Some("orca"), Some("fix-login-eyzflow"), &Value::Null)
        );
        assert!(r.get("/v1/actions").1["actions"].as_array().unwrap().is_empty(), "a dry run starts nothing");
        // What can't be planned is said at once.
        for (body, status, want) in [
            ("{}", 400, "empty"),
            (r#"{"goal":"  "}"#, 400, "empty"),
            ("nope", 400, "bad_json"),
            (r#"{"goal":"x","setup":"nobody's"}"#, 404, "no_setup"),
        ] {
            let (st, v) = r.post("/v1/launch", body);
            assert_eq!((st, code(&v)), (status, Some(want)), "{body}: {v}");
        }

        // For real: the plan and an id at once, though no session has opened yet.
        let mut ev = r.events();
        ev.next("missions");
        let asked = Instant::now();
        let (st, v) = r.post("/v1/launch", r#"{"goal":"Fix the login redirect","setup":"eyzflow","name":"Login fix"}"#);
        assert!(asked.elapsed() < Duration::from_secs(5));
        let a = &v["action"];
        let id = a["id"].as_str().unwrap().to_string();
        assert_eq!(
            (st, a["kind"].as_str(), a["state"].as_str(), a["space"].as_str(), &a["receipt"], &a["finished_ms"]),
            (202, Some("launch"), Some("running"), Some("Login fix"), &Value::Null, &Value::Null),
            "{v}"
        );
        assert_eq!(
            (a["plan"]["dry_run"].as_bool(), a["plan"]["assumed"]["name_why"].as_str(), a["plan"]["sessions"].as_array().map(Vec::len)),
            (Some(true), Some("as given"), Some(2))
        );
        assert_eq!(ev.next("action")["id"], id.as_str(), "the stream says it began");
        assert_eq!(r.get(&format!("/v1/actions/{id}")).1["action"]["state"], "running");
        // One at a time.
        let (st, v) = r.post("/v1/launch", r#"{"goal":"Another thing"}"#);
        assert_eq!((st, code(&v), v["error"]["action"].as_str()), (409, Some("launching"), Some(id.as_str())), "{v}");
        // Each session that opens is a step: the receipt so far.
        r.go.send(()).unwrap();
        let step = ev.next("action");
        assert_eq!(
            (step["state"].as_str(), step["receipt"]["sessions"][0]["address"].as_str(), &step["receipt"]["sessions"][1]["address"]),
            (Some("running"), Some("t40"), &Value::Null),
            "{step}"
        );
        r.go.send(()).unwrap();
        let end = loop {
            let e = ev.next("action");
            if e["state"] != "running" {
                break e;
            }
        };
        assert_eq!(
            (
                end["state"].as_str(),
                end["receipt"]["agenda"].as_str(),
                end["receipt"]["dry_run"].as_bool(),
                end["receipt"]["sessions"][1]["address"].as_str()
            ),
            (Some("done"), Some("a1"), Some(false), Some("t41")),
            "{end}"
        );
        assert_eq!(r.finished(&id), end, "the same by its id");
        assert!(ev.next("missions")["spaces"].as_array().unwrap().iter().any(|s| s["name"] == "Login fix"), "and the space is a mission");
        // No session opens: the launch failed, and its receipt says why for each.
        r.tower.fails.store(true, Ordering::SeqCst);
        let (_, v) = r.post("/v1/launch", r#"{"goal":"Ship checkout v2"}"#);
        let id2 = v["action"]["id"].as_str().unwrap().to_string();
        r.go.send(()).unwrap();
        r.go.send(()).unwrap();
        let failed = r.finished(&id2);
        assert_eq!(
            (failed["state"].as_str(), failed["error"].as_str(), failed["receipt"]["sessions"][0]["error"].as_str()),
            (Some("failed"), Some("no session of \"ship-checkout\" opened"), Some("its folder is gone")),
            "{failed}"
        );
        // Both are listed, newest first; an id nobody has is a 404.
        let (_, list) = r.get("/v1/actions");
        assert_eq!(
            list["actions"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect::<Vec<_>>(),
            [id2, id]
        );
        let (st, v) = r.get("/v1/actions/act_00000000");
        assert_eq!((st, code(&v)), (404, Some("no_action")));
        r.end();
    }

    #[test]
    fn the_tell_level_the_model_and_a_request_in_a_space() {
        let r = rig("settings");
        for (m, p) in [("GET", "/v1/tell"), ("POST", "/v1/tell"), ("GET", "/v1/model"), ("POST", "/v1/model"), ("GET", "/v1/thread")] {
            assert_eq!(r.stranger(m, p), 401, "{m} {p}");
        }
        // How much the tower tells.
        assert_eq!(r.get("/v1/tell"), (200, json!({ "level": "decisions", "voice": "replies" })));
        assert_eq!(r.post("/v1/tell", r#"{"level":"results"}"#), (200, json!({ "level": "results", "was": "decisions", "voice": "replies" })));
        assert_eq!(
            (r.get("/v1/tell").1["level"].as_str(), r.get("/v1/state").1["tell_level"].as_str()),
            (Some("results"), Some("results"))
        );
        for body in [r#"{"level":"loud"}"#, "{}", r#"{"level":2}"#] {
            let (st, v) = r.post("/v1/tell", body);
            assert_eq!((st, code(&v)), (400, Some("bad_level")), "{body}");
        }
        assert_eq!(r.get("/v1/tell").1["level"], "results", "a level it doesn't know changes nothing");
        // What it says out loud: off, decisions, or replies. Alone, or with the level.
        assert_eq!(r.post("/v1/tell", r#"{"voice":"decisions"}"#), (200, json!({ "level": "results", "voice": "decisions", "voice_was": "replies" })));
        assert_eq!((r.get("/v1/tell").1["voice"].as_str(), r.get("/v1/state").1["tell_voice"].as_str()), (Some("decisions"), Some("decisions")));
        let (st, v) = r.post("/v1/tell", r#"{"level":"decisions","voice":"off"}"#);
        assert_eq!((st, v), (200, json!({ "level": "decisions", "was": "results", "voice": "off", "voice_was": "decisions" })));
        assert_eq!(r.get("/v1/state").1["muted"], true, "off is the mute");
        for body in [r#"{"voice":"loud"}"#, r#"{"voice":1}"#, r#"{"level":"results","voice":"loud"}"#] {
            let (st, v) = r.post("/v1/tell", body);
            assert_eq!((st, code(&v)), (400, Some("bad_voice")), "{body}");
        }
        assert_eq!(r.get("/v1/tell").1, json!({ "level": "decisions", "voice": "off" }), "a word it doesn't know changes nothing");
        r.post("/v1/tell", r#"{"level":"results","voice":"replies"}"#);

        // The model the brains run on.
        let (st, m) = r.get("/v1/model");
        assert_eq!(
            (st, m["program"].as_str(), m["model"].as_str(), m["effort"].as_str()),
            (200, Some("claude"), Some(crate::brains::DEFAULT_MODEL), Some(crate::brains::DEFAULT_EFFORT)),
            "{m}"
        );
        assert_eq!(
            (m["choices"]["models"].as_array().map(Vec::len), m["choices"]["efforts"].as_array().map(Vec::len), &m["restarted"]),
            (Some(4), Some(5), &json!([]))
        );
        for k in ["running", "restart_needed", "locked_by_args", "locked", "says"] {
            assert!(m.get(k).is_some(), "the report has {k}");
        }
        let (st, m) = r.post("/v1/model", r#"{"model":"Fable 5.2","effort":"XHIGH"}"#);
        assert_eq!(
            (st, m["model"].as_str(), m["effort"].as_str(), &m["restarted"]),
            (200, Some("claude-fable-5-2"), Some("xhigh"), &json!([])),
            "by its picker name too: {m}"
        );
        assert_eq!(r.post("/v1/model", r#"{"restart":true}"#).1["restarted"], json!(["tower"]));
        assert_eq!(r.get("/v1/state").1["brain_model"]["model"], "claude-fable-5-2", "the state has the report for the picker");
        for (body, want) in [
            (r#"{"effort":"extreme"}"#, "bad_effort"),
            (r#"{"model":"--dangerous"}"#, "bad_model"),
            (r#"{"model":3}"#, "bad_model"),
            ("{}", "bad_model"),
            ("nope", "bad_json"),
        ] {
            let (st, v) = r.post("/v1/model", body);
            assert_eq!((st, code(&v)), (400, Some(want)), "{body}");
        }
        assert_eq!(r.get("/v1/model").1["effort"], "xhigh");

        // Asked in a space's view: that space goes with it, by its whole name in any case.
        let (st, v) = r.post("/v1/ask", r#"{"text":"what's left here?","space":"b2c support"}"#);
        assert_eq!((st, v["sent"].as_str(), v["space"].as_str()), (202, Some("what's left here?"), Some("B2C support")), "{v}");
        let phone = Some(r.device.clone());
        let source = format!("phone:{}", r.device);
        assert_eq!(
            *r.tower.asked.lock().unwrap(),
            [("what's left here?".to_string(), Some("B2C support".to_string()), phone.clone(), source.clone())]
        );
        // The same as a form.
        let body = multipart::build("XB", &[("text", "and the tickets?"), ("space", "Q&A / ünï")], &[]);
        let (st, v) = call_raw(r.addr, &r.fp, "POST", "/v1/ask", &r.token, "multipart/form-data; boundary=XB", &body).unwrap();
        assert_eq!((st, v["space"].as_str()), (202, Some("Q&A / ünï")), "{v}");
        // Asked of the tower itself: no space, as before.
        let (st, v) = r.post("/v1/ask", r#"{"text":"what's everyone doing?"}"#);
        assert_eq!((st, v.get("space"), v["attachments"].as_array().map(Vec::len)), (202, None, Some(0)), "{v}");
        // A space nobody has: nothing is asked of anyone.
        let (st, v) = r.post("/v1/ask", r#"{"text":"hello?","space":"Nowhere"}"#);
        assert_eq!((st, code(&v)), (404, Some("no_space")));
        assert_eq!(code(&r.post("/v1/ask", r#"{"text":" ","space":"Shop"}"#).1), Some("empty"));
        let asked = r.tower.asked.lock().unwrap().clone();
        assert_eq!(
            asked.iter().map(|a| (a.0.as_str(), a.1.as_deref())).collect::<Vec<_>>(),
            [("what's left here?", Some("B2C support")), ("and the tickets?", Some("Q&A / ünï")), ("what's everyone doing?", None)]
        );
        // A phone whose replies are heard on the Mac asks the same way, and no phone hears it alone.
        assert_eq!(call(r.addr, &r.fp, "PUT", "/v1/device/audio", Some(&r.token), r#"{"route":"mac"}"#).unwrap().0, 200);
        assert_eq!(r.post("/v1/agents/t22/answer", r#"{"decision":"approve","note":"only the read"}"#).0, 202);
        assert_eq!(
            r.tower.asked.lock().unwrap().last().cloned(),
            Some(("Approve what t22 is asking for. only the read".to_string(), None, None, source))
        );

        // What an app checks first, and the calls from before this still answer.
        let (st, hello) = r.get("/v1/hello");
        assert_eq!((st, hello["api"].as_u64(), &hello["features"]), (200, Some(1), &json!(["quiet", "acts", "pairing", "bonjour", "reviews", "scores", "validation", "impact", "releases", "accounts", "jobs", "browsers"])), "{hello}");
        let (_, state) = r.get("/v1/state");
        for k in [
            "version",
            "phase",
            "mode",
            "agents",
            "questions",
            "question_policy",
            "decisions_count",
            "tell_level",
            "brain_model",
            "brains",
            "work",
            "audio_route",
        ] {
            assert!(state.get(k).is_some(), "the state has {k}");
        }
        for (path, key) in [
            ("/v1/questions", "questions"),
            ("/v1/prs", "prs"),
            ("/v1/chat", "messages"),
            ("/v1/history", "entries"),
            ("/v1/jobs", "jobs"),
            ("/v1/queue", "items"),
            ("/v1/mute", "muted"),
            ("/v1/audio", "clips"),
            ("/v1/screenshots", "screenshots"),
            ("/v1/questions-policy", "policy"),
        ] {
            let (st, v) = r.get(path);
            assert!(st == 200 && v.get(key).is_some(), "{path}: {st} {v}");
        }
        assert_eq!(r.get("/v1/nope").0, 404);
        r.end();
    }

    #[test]
    fn the_record_a_thread_and_each_conversation_come_through() {
        let r = rig("record");
        let lines = [
            json!({ "at": 100, "kind": "request", "text": "what's everyone doing?", "source": "mac" }),
            json!({ "at": 110, "kind": "answer", "text": "Nothing is blocked." }),
            json!({ "at": 120, "kind": "peer", "session": "t6", "name": "api", "from": "t5", "from_name": "web", "space": "Shop", "text": "Which port?", "status": "delivered", "record": true }),
            json!({ "at": 130, "kind": "peer", "session": "t5", "name": "web", "from": "t6", "from_name": "api", "space": "Shop", "text": "3001.", "status": "delivered", "record": true }),
            json!({ "at": 140, "kind": "peer", "session": "t9", "name": "guide", "from": "t8", "from_name": "faq", "space": "Docs", "text": "Is the guide renamed?", "status": "undelivered", "record": true }),
            json!({ "at": 150, "kind": "note", "session": "t6", "name": "api", "space": "Shop", "text": "The api session finished the login fix.", "record": true }),
            json!({ "at": 160, "kind": "raised", "id": "d0123456789ab", "session": "t6", "name": "api", "space": "Shop", "why": "deploy", "text": "Deploy the tickets change?", "options": [{ "label": "Deploy" }] }),
            json!({ "at": 170, "kind": "decision", "id": "d0123456789ab", "space": "Shop", "status": "answered", "why": "deploy", "text": "Deploy the tickets change? -> Deploy", "record": true }),
            json!({ "at": 180, "kind": "request", "text": "what's left here?", "source": "phone:d_1", "space": "Shop" }),
            json!({ "at": 190, "kind": "answer", "text": "Two tasks are left.", "space": "Shop" }),
            json!({ "at": 200, "kind": "request", "text": "In B2C support: any news?", "source": "mac" }),
            json!({ "at": 210, "kind": "answer", "text": "No news there." }),
            json!({ "at": 220, "kind": "receipt", "text": "The login is fixed.", "agenda": "a3", "space": "Shop" }),
            json!({ "at": 230, "kind": "peer", "session": "t6", "name": "api", "from": "t5", "from_name": "web", "space": "Shop", "text": "Thanks.", "status": "delivered", "record": true }),
            json!({ "at": 240, "kind": "space", "space": "Shop", "status": "closed", "text": "Shop is closed. The login is fixed.", "record": true }),
        ];
        std::fs::write(&r.history, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let kinds =
            |v: &Value| v["entries"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>();

        // The Record of one space: every kind, with what each carries.
        let (st, shop) = r.get("/v1/history?space=shop");
        assert_eq!(
            (st, kinds(&shop)),
            (
                200,
                ["peer", "peer", "note", "raised", "decision", "request", "answer", "receipt", "peer", "space"].map(String::from).to_vec()
            ),
            "{shop}"
        );
        let e = |kind: &str| shop["entries"].as_array().unwrap().iter().find(|e| e["kind"] == kind).unwrap().clone();
        assert_eq!(
            (e("peer")["from"].as_str(), e("peer")["from_name"].as_str(), e("peer")["record"].as_bool(), e("peer")["status"].as_str()),
            (Some("t5"), Some("web"), Some(true), Some("delivered"))
        );
        assert_eq!(
            (e("raised")["why"].as_str(), e("raised")["id"].as_str(), e("raised").get("record")),
            (Some("deploy"), Some("d0123456789ab"), None),
            "raised the first time: told, so not only kept"
        );
        assert_eq!(
            (
                e("note")["record"].as_bool(),
                e("decision")["status"].as_str(),
                e("receipt")["agenda"].as_str(),
                e("space")["status"].as_str()
            ),
            (Some(true), Some("answered"), Some("a3"), Some("closed"))
        );
        // By kind, and both.
        assert_eq!(kinds(&r.get("/v1/history?kind=peer").1).len(), 4);
        assert_eq!(kinds(&r.get("/v1/history?kind=peer,raised&space=Shop").1), ["peer", "peer", "raised", "peer"]);
        assert_eq!(kinds(&r.get("/v1/history?kind=note,receipt,decision,space").1), ["note", "decision", "receipt", "space"]);
        assert_eq!(kinds(&r.get("/v1/history?space=Shop&limit=2").1), ["peer", "space"], "the latest, oldest first");
        assert_eq!(kinds(&r.get("/v1/history?space=Shop&limit=2&before_ms=230").1), ["answer", "receipt"]);
        assert!(kinds(&r.get("/v1/history?space=Sho").1).is_empty() && kinds(&r.get("/v1/history?space=B2C+support").1).is_empty());
        assert_eq!(kinds(&r.get("/v1/history?session=t6&kind=peer").1).len(), 2, "by session, as before");

        // A space's thread, newest last, in pages like the chat's.
        let texts =
            |v: &Value| v["messages"].as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap().to_string()).collect::<Vec<_>>();
        let (st, page) = r.get("/v1/thread?space=Shop&limit=2");
        assert_eq!(
            (st, texts(&page), page["more"].as_bool(), page["space"].as_str()),
            (200, vec!["3001.".to_string(), "Thanks.".to_string()], Some(true), Some("Shop")),
            "{page}"
        );
        let m = &page["messages"][0];
        assert_eq!(
            *m,
            json!({ "id": "h4", "at": 130, "from": "t6", "from_name": "api", "to": "t5", "to_name": "web", "text": "3001.", "space": "Shop", "delivered": true })
        );
        let (_, older) = r.get("/v1/thread?space=shop&limit=2&before=h4");
        assert_eq!((texts(&older), older["more"].as_bool()), (vec!["Which port?".to_string()], Some(false)));
        let (_, docs) = r.get("/v1/thread?space=Docs");
        assert_eq!((texts(&docs).len(), docs["messages"][0]["delivered"].as_bool()), (1, Some(false)));
        assert_eq!(texts(&r.get("/v1/thread").1).len(), 4, "no space: every space's");
        let (st, none) = r.get("/v1/thread?space=B2C%20support");
        assert_eq!((st, texts(&none).len(), none["more"].as_bool()), (200, 0, Some(false)), "nobody wrote there yet: empty, not an error");

        // The chat says whose conversation a request and its answer are.
        let (_, chat) = r.get("/v1/chat");
        let of = |id: &str| {
            chat["messages"].as_array().unwrap().iter().find(|m| m["id"] == id).unwrap_or_else(|| panic!("{id} in {chat}"))["space"].clone()
        };
        assert_eq!((of("h1"), of("h2"), of("h9"), of("h10")), (Value::Null, Value::Null, json!("Shop"), json!("Shop")));
        assert_eq!(
            (of("h11"), of("h12"), of("h13")),
            (json!("B2C support"), json!("B2C support"), json!("Shop")),
            "asked of the main tower about a space; and a receipt names its request's"
        );
        let ids = |path: &str| {
            r.get(path).1["messages"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(
            ids("/v1/chat"),
            ["h1", "h2", "h9", "h10", "h11", "h12", "h13"],
            "all of it, as an app from before this gets it (the record is not the chat)"
        );
        assert_eq!(ids("/v1/chat?space=Shop"), ["h9", "h10", "h13"]);
        assert_eq!(ids("/v1/chat?space=b2c%20support"), ["h11", "h12"]);
        assert_eq!(ids("/v1/chat?main=1"), ["h1", "h2", "h13"], "the main one: clean of the spaces' own, with the receipt");
        assert_eq!(ids("/v1/chat?space=Shop&limit=1&before=h13"), ["h10"]);
        r.end();
    }

    /// A take spoken in a space's view goes to that space's tower, as a
    /// typed request there does (`/v1/ask` with `space`); a name no space
    /// has is refused before anything is transcribed.
    #[test]
    fn a_spoken_request_in_a_space_goes_to_its_tower() {
        let r = rig("voice-space");
        let wav = super::super::audio::wav_encode(&vec![0.0f32; 8_000], 16_000);
        let post = |path: &str| call_raw(r.addr, &r.fp, "POST", path, &r.token, "audio/wav", &wav).unwrap();
        // Heard as the transcriber would: what comes in, with where it was said.
        let hear = |said: &'static str| {
            let rx = r.stt.lock().unwrap();
            let u = rx.recv_timeout(Duration::from_secs(10)).expect("a take for the transcriber");
            match u.source {
                crate::daemon::mic::Source::Phone { reply, send, space, device, .. } => {
                    reply.send(said.to_string()).unwrap();
                    (space, send, device)
                }
                _ => panic!("not the phone's"),
            }
        };
        let (st, v) = post("/v1/voice?space=Nowhere");
        assert_eq!((st, code(&v)), (404, Some("no_space")), "{v}");
        assert!(r.stt.lock().unwrap().try_recv().is_err(), "nothing was handed on");
        let (got, answer) = thread::scope(|s| {
            let call = s.spawn(|| post("/v1/voice?space=b2c%20support"));
            let got = hear("what's left here");
            (got, call.join().unwrap())
        });
        assert_eq!(got, (Some("B2C support".to_string()), true, r.device.clone()), "by its whole name, in any case");
        assert_eq!(answer.0, 200);
        assert_eq!((answer.1["heard"].as_str(), answer.1["sent"].as_bool(), answer.1["space"].as_str()), (Some("what's left here"), Some(true), Some("B2C support")));
        // Said to the tower itself: no space, as before.
        let (got, answer) = thread::scope(|s| {
            let call = s.spawn(|| post("/v1/voice"));
            (hear("what's everyone doing"), call.join().unwrap())
        });
        assert_eq!((got.0, answer.0, answer.1.get("space")), (None, 200, None));
        // Dictation takes no space: only the words come back.
        let (got, answer) = thread::scope(|s| {
            let call = s.spawn(|| post("/v1/voice?send=0&space=Nowhere"));
            (hear("a draft"), call.join().unwrap())
        });
        assert_eq!((got.0, got.1, answer.0, answer.1["sent"].as_bool()), (None, false, 200, Some(false)));
        r.end();
    }

    /// A space's own conversation pages back like the main chat, and what the
    /// tower opened in answer to a request asked there (an act) is in it, and
    /// not in the main one: asked of its own tower, or of the main tower with
    /// the space named in front.
    #[test]
    fn a_spaces_conversation_pages_back_and_holds_its_acts() {
        let r = rig("space-chat");
        let act = |id: &str, at: u64, asked_at: u64, asked_in: Option<&str>| {
            let a = crate::acts::Act {
                id: id.into(),
                kind: "start".into(),
                at_ms: at,
                by: "tower".into(),
                space: Some("Shop".into()),
                sessions: vec![crate::acts::Opened { address: "t9".into(), name: "cart fix".into(), ..Default::default() }],
                request: Some(crate::acts::Asked { at: asked_at, text: "open a session".into(), space: asked_in.map(String::from) }),
                state: "done".into(),
                ..Default::default()
            };
            let mut e = a.entry();
            e.at = at;
            serde_json::to_value(e).unwrap()
        };
        let mut lines = vec![
            json!({ "at": 100, "kind": "request", "text": "what's everyone doing?", "source": "mac" }),
            json!({ "at": 110, "kind": "answer", "text": "Nothing is blocked." }),
            json!({ "at": 120, "kind": "request", "text": "open a session for the cart fix", "source": "phone:d_1", "space": "Shop" }),
            act("r000000000001", 125, 120, Some("Shop")),
            json!({ "at": 130, "kind": "answer", "text": "Opened cart fix in Shop.", "space": "Shop" }),
            json!({ "at": 140, "kind": "request", "text": "In B2C support: open a session for the refunds", "source": "mac" }),
            act("r000000000002", 145, 140, None),
            json!({ "at": 150, "kind": "answer", "text": "Opened refunds." }),
        ];
        for i in 0..6u64 {
            lines.push(json!({ "at": 200 + 10 * i, "kind": if i % 2 == 0 { "request" } else { "answer" }, "text": format!("shop {i}"), "space": "Shop", "source": "mac" }));
        }
        std::fs::write(&r.history, lines.iter().map(|l| l.to_string() + "\n").collect::<String>()).unwrap();
        let page = |path: &str| {
            let (st, v) = r.get(path);
            assert_eq!(st, 200, "{path}: {v}");
            let ids = v["messages"].as_array().unwrap().iter().map(|m| format!("{}:{}", m["id"].as_str().unwrap(), m["kind"].as_str().unwrap())).collect::<Vec<_>>();
            (ids, v["more"].as_bool().unwrap())
        };
        let (shop, more) = page("/v1/chat?space=shop");
        assert_eq!(shop, ["h3:request", "h4:act", "h5:answer", "h9:request", "h10:answer", "h11:request", "h12:answer", "h13:request", "h14:answer"]);
        assert!(!more);
        // Back, a page at a time, as the main chat pages.
        let (p1, more1) = page("/v1/chat?space=Shop&limit=4");
        assert_eq!((p1, more1), (vec!["h11:request".to_string(), "h12:answer".into(), "h13:request".into(), "h14:answer".into()], true));
        let (p2, more2) = page("/v1/chat?space=Shop&limit=4&before=h11");
        assert_eq!((p2, more2), (vec!["h4:act".to_string(), "h5:answer".into(), "h9:request".into(), "h10:answer".into()], true));
        let (p3, more3) = page("/v1/chat?space=Shop&limit=4&before=h4");
        assert_eq!((p3, more3), (vec!["h3:request".to_string()], false));
        // The act of the request asked of the main tower for B2C support is that conversation's.
        let (b2c, _) = page("/v1/chat?space=B2C%20support");
        assert_eq!(b2c, ["h6:request", "h7:act", "h8:answer"]);
        // And the main one has neither act.
        let (main, _) = page("/v1/chat?main=1");
        assert_eq!(main, ["h1:request", "h2:answer"]);
        let (_, v) = r.get("/v1/chat?space=Shop&limit=1&before=h5");
        assert_eq!((v["messages"][0]["act"]["id"].as_str(), v["messages"][0]["space"].as_str()), (Some("r000000000001"), Some("Shop")));
        r.end();
    }

    #[test]
    fn a_tower_that_is_still_starting_says_so() {
        let dir = std::env::temp_dir().join(format!("tower-lan-quiet-starting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let hub = Arc::new(Hub::default());
        let (stt, rx) = channel();
        std::mem::forget(rx);
        let shared = Shared::new(State::new("ptt", "af_bella", "test"));
        let asks = Asks::new(shared.clone(), hub.clone(), "normal");
        let lan = Lan::with_paths(
            Ctx { shared, mouth: Mouth::silent(hub.clone()), hub, stt, asks, prs: test_prs() },
            dir.join("devices.json"),
            dir.join("lan"),
        )
        .with_history(dir.join("history.jsonl"));
        let addr = lan
            .start(&crate::config::Lan { enabled: true, port: 0, bind: "127.0.0.1".into(), allow_public: false, tailscale: "off".into(), ..Default::default() })
            .unwrap();
        let fp = lan.info()["fingerprint"].as_str().unwrap().to_string();
        let code_ = lan.pair().unwrap()["code"].as_str().unwrap().to_string();
        let token = call(addr, &fp, "POST", "/v1/pair", None, &format!(r#"{{"code":"{code_}"}}"#)).unwrap().1["token"]
            .as_str()
            .unwrap()
            .to_string();
        for (m, p, body) in [
            ("GET", "/v1/decisions", ""),
            ("GET", "/v1/missions", ""),
            ("POST", "/v1/launch", r#"{"goal":"x"}"#),
            ("GET", "/v1/tell", ""),
            ("POST", "/v1/ask", r#"{"text":"hello"}"#),
            ("POST", "/v1/spaces/Shop/goal", r#"{"goal":"x"}"#),
        ] {
            let (st, v) = call(addr, &fp, m, p, Some(&token), body).unwrap();
            assert_eq!((st, code(&v)), (503, Some("starting")), "{m} {p}");
        }
        // What needs no more than the history and the state still answers.
        assert_eq!(call(addr, &fp, "GET", "/v1/thread", Some(&token), "").unwrap().0, 200);
        assert_eq!(call(addr, &fp, "GET", "/v1/chat", Some(&token), "").unwrap().0, 200);
        assert_eq!(call(addr, &fp, "GET", "/v1/nope", Some(&token), "").unwrap().0, 404);
        lan.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
