//! The colony tools workers see (MCP). Implements only the stateless form of Streamable HTTP:
//! one JSON-RPC request per POST, answered with `application/json`. No sessions, no SSE.
//! Workers are identified by `x-colony-agent`, which claude sends from the `headers` of its MCP config.

use std::time::Duration;

use serde_json::{json, Value};

use crate::board::{short, BoardError, Claim, CommitCheck, GoalStatus, NewTask};
use crate::hub;
use crate::server::{Shared, VerifyRun};
use crate::supervisor::BoardSignal;

/// Protocol version to answer with when the client sends none.
const PROTOCOL_FALLBACK: &str = "2025-06-18";
const VERIFY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Handles one request body. Notifications (no response) yield `None` → HTTP 202.
pub async fn handle(app: &Shared, agent: Option<&str>, body: &[u8]) -> Option<Value> {
    let msg: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    if let Value::Array(batch) = msg {
        let mut out = Vec::new();
        for m in &batch {
            if let Some(r) = dispatch(app, agent, m).await {
                out.push(r);
            }
        }
        return (!out.is_empty()).then_some(Value::Array(out));
    }
    dispatch(app, agent, &msg).await
}

async fn dispatch(app: &Shared, agent: Option<&str>, req: &Value) -> Option<Value> {
    let id = req.get("id")?.clone();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_FALLBACK),
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "colony", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "Shared board, chat and notes of this colony. Start every turn with board_view.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_list() })),
        "tools/call" => match agent {
            None => Err((-32602, "missing x-colony-agent header".to_string())),
            Some(agent) => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
                let (text, is_error) = match call_tool(app, agent, name, &args).await {
                    Ok(t) => (t, false),
                    Err(e) => (e, true),
                };
                app.log_tool(agent, name, &args, &text, is_error);
                Ok(json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
            }
        },
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, msg)) => rpc_error(id, code, &msg),
    })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

type ToolResult = Result<String, String>;

fn err(e: BoardError) -> String {
    e.to_string()
}

/// Argument names models send instead of the schema's. The same aliases recurred across all three real
/// runs (`message` for chat_post, `task_id` for task_claim, `note` for task_progress). Refusing them only
/// burns turns, so they are accepted.
fn aliases(key: &str) -> &'static [&'static str] {
    match key {
        "id" => &["task_id", "taskId"],
        "text" => &["message", "note", "content"],
        "summary" => &["text", "message", "note"],
        "reason" => &["text", "message", "note"],
        "plan" => &["text", "note"],
        "body" => &["text", "content"],
        "commit" => &["sha", "commit_sha"],
        "query" => &["q", "text"],
        _ => &[],
    }
}

fn arg(args: &Value, key: &str) -> Result<String, String> {
    std::iter::once(key)
        .chain(aliases(key).iter().copied())
        .find_map(|k| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string)
        .ok_or_else(|| format!("`{key}` is required"))
}

fn opt_arg(args: &Value, key: &str) -> Option<String> {
    arg(args, key).ok()
}

fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Wakes idle workers only when the change opened up claimable work. Waking everyone on every change bought
/// turns that found nothing to do (observed: 41 of 50 turns in a real run held no task).
fn signal_if_new_work(app: &Shared, before: &[String]) {
    let after = app.board().claimable_ids();
    if after.iter().any(|id| !before.contains(id)) {
        app.signal(BoardSignal::NewWork);
    }
}

/// Wakes the workers a message addresses by name. `@all` is an announcement read at the next turn start
/// and wakes nobody.
fn signal_direct(app: &Shared, mentions: &[String]) {
    let direct: Vec<String> = mentions.iter().filter(|m| *m != "all").cloned().collect();
    if !direct.is_empty() {
        app.signal(BoardSignal::Mention(direct));
    }
}

/// The `task_complete` answer. One task per turn: the protocol said to end the turn, but the old answer said
/// "pick your next task", and workers ran 2–5 tasks per 20-minute turn (observed). Ending the turn lets the
/// harness account cost, deliver messages and start a fresh session. Agensh also reminds a worker whose turn
/// published nothing to shared context.
fn complete_reply(id: &str, noted: bool) -> String {
    let mut s = format!("{id} is done. End your turn now; your next turn starts with a fresh board digest.");
    if !noted {
        s.push_str(
            " You added no note for this task: if you learned anything another worker could reuse (a \
             convention, a command, a gotcha, an approach that failed), note_add it before ending your turn.",
        );
    }
    s
}

/// `task_propose` input. The schema says `{tasks: [...]}`, but models sometimes send a single task flat as
/// `{title, description}` or send `tasks` as a JSON string (observed in real runs), so those are accepted too.
fn proposed_tasks(args: &Value) -> Result<Vec<NewTask>, String> {
    let list: Vec<Value> = match args.get("tasks") {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            _ => return Err("`tasks` must be an array of {title, detail, deps}".into()),
        },
        Some(_) => return Err("`tasks` must be an array of {title, detail, deps}".into()),
        None if args.get("title").is_some() => vec![args.clone()],
        None => return Err("`tasks` must be an array of {title, detail, deps}".into()),
    };
    let text = |t: &Value, keys: &[&str]| {
        keys.iter()
            .find_map(|k| t.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    Ok(list
        .iter()
        .map(|t| NewTask {
            title: text(t, &["title"]),
            detail: text(t, &["detail", "description"]),
            deps: match t.get("deps") {
                Some(Value::Array(d)) => d.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                Some(Value::String(d)) if !d.trim().is_empty() => {
                    d.split(',').map(|x| x.trim().to_string()).collect()
                }
                _ => Vec::new(),
            },
        })
        .collect())
}

async fn call_tool(app: &Shared, agent: &str, name: &str, args: &Value) -> ToolResult {
    match name {
        "board_view" => {
            let head = app.hub.head().await.unwrap_or_default();
            Ok(app.board().render_view(agent, &head))
        }
        "task_get" => app.board().render_task(&arg(args, "id")?).map_err(err),
        "task_propose" => {
            let items = proposed_tasks(args)?;
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let before = app.board().claimable_ids();
            let ids = app.board().propose(agent, items, force, now()).map_err(err)?;
            signal_if_new_work(app, &before);
            Ok(format!(
                "proposed {}. Claim one with task_claim if you hold nothing.",
                ids.join(", ")
            ))
        }
        "task_claim" => {
            let id = arg(args, "id")?;
            let plan = opt_arg(args, "plan").unwrap_or_default();
            match app.board().claim(agent, &id, &plan, now()).map_err(err)? {
                Claim::Held => Ok(format!(
                    "you hold {id}. Work in your clone; when it is done and checked, push to {} and call task_complete.",
                    hub::BRANCH
                )),
                Claim::Reserved { waits } => Ok(format!(
                    "you reserve {id}; it waits on {}. It becomes yours automatically when they are done (if you \
                     hold no other task then), and you will be woken. Prepare locally if useful, but push only \
                     once it is yours. task_release {id} drops the reservation.",
                    waits.join(", ")
                )),
            }
        }
        "task_progress" => {
            let id = arg(args, "id")?;
            app.board()
                .progress(agent, &id, &arg(args, "text")?, now())
                .map_err(err)?;
            Ok(format!("logged on {id}"))
        }
        "task_release" => {
            let id = arg(args, "id")?;
            let before = app.board().claimable_ids();
            app.board()
                .release(agent, &id, &arg(args, "reason")?, now())
                .map_err(err)?;
            signal_if_new_work(app, &before);
            Ok(format!("released {id}; it is open again"))
        }
        "task_complete" => {
            let id = arg(args, "id")?;
            let summary = arg(args, "summary")?;
            let check = match opt_arg(args, "commit") {
                None => None,
                Some(c) => {
                    let merged = app.hub.merged_sha(&c).await.map_err(|e| e.to_string())?;
                    Some(match merged {
                        Some(full) => CommitCheck {
                            sha: full,
                            merged: true,
                        },
                        None => CommitCheck {
                            sha: c,
                            merged: false,
                        },
                    })
                }
            };
            let before = app.board().claimable_ids();
            let (auto, all_done, noted) = {
                let mut b = app.board();
                let auto = b.complete(agent, &id, &summary, check, now()).map_err(err)?;
                (auto, b.is_closable(), b.noted_since_claim(agent, &id))
            };
            app.signal(BoardSignal::TaskDone {
                agent: agent.to_string(),
                all_done,
            });
            signal_direct(app, &auto.into_iter().map(|a| a.agent).collect::<Vec<_>>());
            signal_if_new_work(app, &before);
            Ok(complete_reply(&id, noted))
        }
        "task_reopen" => {
            let id = arg(args, "id")?;
            let reason = arg(args, "reason")?;
            let before = app.board().claimable_ids();
            let mention = {
                let mut b = app.board();
                let owner = b.task(&id).and_then(|t| t.claimed_by.clone());
                b.reopen(agent, &id, &reason, now()).map_err(err)?;
                let text = match &owner {
                    Some(o) if o != agent => format!("@{o} {agent} reopened {id}: {reason}"),
                    _ => format!("{agent} reopened {id}: {reason}"),
                };
                b.post(agent, &text, now()).map_err(err)?.mentions
            };
            signal_if_new_work(app, &before);
            signal_direct(app, &mention);
            Ok(format!("reopened {id}"))
        }
        "chat_post" => {
            let msg = app.board().post(agent, &arg(args, "text")?, now()).map_err(err)?;
            signal_direct(app, &msg.mentions);
            Ok(format!("posted #{}", msg.seq))
        }
        "chat_read" => {
            let (msgs, skipped) = app.board().read_chat(agent);
            if msgs.is_empty() {
                return Ok("no unread messages".into());
            }
            let mut out = String::new();
            if skipped > 0 {
                out.push_str(&format!("({skipped} older unread messages skipped)\n"));
            }
            for m in msgs {
                out.push_str(&format!("#{} {}: {}\n", m.seq, m.from, m.text));
            }
            Ok(out)
        }
        "note_add" => {
            let tags = args
                .get("tags")
                .and_then(Value::as_array)
                .map(|t| t.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            let body = opt_arg(args, "body").unwrap_or_default();
            let id = app
                .board()
                .add_note(agent, &arg(args, "title")?, &body, tags, now())
                .map_err(err)?;
            Ok(format!("saved {id}"))
        }
        "note_search" => {
            let query = opt_arg(args, "query").unwrap_or_default();
            let b = app.board();
            let notes = b.search_notes(&query, 8);
            if notes.is_empty() {
                return Ok("no matching notes".into());
            }
            let mut out = String::new();
            for n in notes {
                out.push_str(&format!("## {} {} (by {}", n.id, n.title, n.by));
                if !n.tags.is_empty() {
                    out.push_str(&format!("; {}", n.tags.join(", ")));
                }
                out.push_str(")\n");
                out.push_str(&crate::board::clip(&n.body, 1500));
                out.push_str("\n\n");
            }
            Ok(out)
        }
        "verify" => {
            let (run, cached) = verify_main(app, agent).await?;
            Ok(describe_verify(app, &run, cached))
        }
        "goal_complete" => {
            let summary = opt_arg(args, "summary").unwrap_or_else(|| "(no summary given)".into());
            goal_complete(app, agent, &summary).await
        }
        _ => Err(format!("unknown tool `{name}`")),
    }
}

/// The cached run for `head`, if any.
fn cached_for<'a>(cache: &'a Option<VerifyRun>, head: &str) -> Option<&'a VerifyRun> {
    cache.as_ref().filter(|r| r.head == head)
}

/// Runs the configured verify command on colony-main's latest commit, or returns the result already recorded
/// for that commit. The bool is true for a cached result.
async fn verify_main(app: &Shared, agent: &str) -> Result<(VerifyRun, bool), String> {
    let Some(v) = &app.verify else {
        return Err("no verify command is configured for this run; check colony-main yourself".into());
    };
    // One run at a time; a second caller waits and then finds the first one's result in the cache.
    let _turn = app.verify_lock.lock().await;
    let head = app.hub.head().await.map_err(|e| format!("{e:#}"))?;
    let hit = app
        .verify_cache
        .lock()
        .ok()
        .and_then(|c| cached_for(&c, &head).cloned());
    if let Some(r) = hit {
        return Ok((r, true));
    }
    let head = hub::sync_to_hub(&v.workspace)
        .await
        .map_err(|e| format!("verify checkout failed: {e:#}"))?;
    let out = {
        let _lease = app.gate.acquire(agent, "verify").await;
        hub::run_shell(&v.workspace, &v.command, VERIFY_TIMEOUT, 60)
            .await
            .map_err(|e| format!("{e:#}"))?
    };
    let run = VerifyRun {
        head,
        ok: out.ok,
        code: out.code,
        tail: out.tail,
        by: agent.to_string(),
        at_ms: now(),
    };
    if let Ok(mut c) = app.verify_cache.lock() {
        *c = Some(run.clone());
    }
    Ok((run, false))
}

fn describe_verify(app: &Shared, run: &VerifyRun, cached: bool) -> String {
    let cmd = app.verify.as_ref().map(|v| v.command.as_str()).unwrap_or("");
    let ago = (now() - run.at_ms).max(0) / 60_000;
    let origin = if cached {
        format!("cached: run by {} {ago}m ago", run.by)
    } else {
        format!("run by {}", run.by)
    };
    if run.ok {
        format!(
            "verify `{cmd}` passed on {} @{} ({origin}).",
            hub::BRANCH,
            short(&run.head)
        )
    } else {
        format!(
            "verify `{cmd}` FAILED on {} @{} (exit {:?}; {origin}). Last lines:\n{}",
            hub::BRANCH,
            short(&run.head),
            run.code,
            run.tail
        )
    }
}

async fn goal_complete(app: &Shared, agent: &str, summary: &str) -> ToolResult {
    app.board().check_closable().map_err(err)?;
    if app.verify.is_some() {
        let (run, cached) = verify_main(app, agent).await?;
        if app.board().goal().status != GoalStatus::Active {
            return Ok("the goal is already closed; end your turn".into());
        }
        if !run.ok {
            // Announce a fresh failure once; a cached one was already announced.
            if !cached {
                let note = format!(
                    "@all goal_complete by {agent} was refused: verify fails on {} @{}. Fixing the branch comes first.",
                    hub::BRANCH,
                    short(&run.head)
                );
                if let Ok(m) = app.board().post("colony", &note, now()) {
                    app.signal(BoardSignal::Mention(m.mentions));
                }
            }
            return Err(describe_verify(app, &run, cached));
        }
    }
    app.board().close_goal(agent, summary).map_err(err)?;
    app.signal(BoardSignal::GoalClosed);
    Ok("goal complete. The harness is stopping all workers; end your turn.".into())
}

pub fn tool_list() -> Value {
    let id = json!({ "type": "string", "description": "task id such as T3" });
    json!([
        {
            "name": "board_view",
            "description": "The shared board: goal status, your claim and reservation, claimable tasks, every task's status, unread chat for you, latest notes. Call it at the start of every turn.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "task_get",
            "description": "Full detail and log (plans, progress, releases, reopen reasons) of one task.",
            "inputSchema": { "type": "object", "properties": { "id": id }, "required": ["id"] }
        },
        {
            "name": "task_propose",
            "description": "Add tasks to the board. Each should be roughly 15-60 minutes of work for one worker. deps lists task ids that must be done first: existing ids (T3) or earlier items of this same call (#1). Near-duplicates of open work are refused unless force is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "detail": { "type": "string", "description": "what done looks like, files or areas involved" },
                                "deps": { "type": "array", "items": { "type": "string" } }
                            },
                            "required": ["title"]
                        }
                    },
                    "force": { "type": "boolean" }
                },
                "required": ["tasks"]
            }
        },
        {
            "name": "task_claim",
            "description": "Atomically claim an open task whose deps are done (you can hold one at a time), or reserve one whose deps are pending: it becomes yours when they are done. The plan is shown to others as your intent.",
            "inputSchema": { "type": "object", "properties": { "id": id, "plan": { "type": "string" } }, "required": ["id", "plan"] }
        },
        {
            "name": "task_progress",
            "description": "Log progress or a decision on the task you hold.",
            "inputSchema": { "type": "object", "properties": { "id": id, "text": { "type": "string" } }, "required": ["id", "text"] }
        },
        {
            "name": "task_release",
            "description": "Give back the task you hold (blocked, wrong, or too big; propose smaller tasks first if so), or drop your reservation. It becomes open again.",
            "inputSchema": { "type": "object", "properties": { "id": id, "reason": { "type": "string" } }, "required": ["id", "reason"] }
        },
        {
            "name": "task_complete",
            "description": "Mark the task you hold done, then end your turn. If you changed code, pass the commit you pushed to colony-main (git rev-parse HEAD after the push); it is refused unless that commit is on colony-main.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": id, "summary": { "type": "string" }, "commit": { "type": "string" } },
                "required": ["id", "summary"]
            }
        },
        {
            "name": "task_reopen",
            "description": "Reopen a done task that is broken or incomplete, with evidence. Its previous owner is notified.",
            "inputSchema": { "type": "object", "properties": { "id": id, "reason": { "type": "string" } }, "required": ["id", "reason"] }
        },
        {
            "name": "chat_post",
            "description": "Post to the shared channel. @w07 reaches that worker even mid-task and wakes it if idle; @all is an announcement read at the next turn and wakes nobody.",
            "inputSchema": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] }
        },
        {
            "name": "chat_read",
            "description": "Read channel messages you have not read yet.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "note_add",
            "description": "Save a reusable finding to shared context: conventions, commands that work, gotchas, where things live, decisions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string" },
                    "body": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["title", "body"]
            }
        },
        {
            "name": "note_search",
            "description": "Search shared notes by words; an empty query lists the latest.",
            "inputSchema": { "type": "object", "properties": { "query": { "type": "string" } } }
        },
        {
            "name": "verify",
            "description": "Run the configured verify command on colony-main's latest commit and report pass or fail with the output tail. Results are cached per commit, so asking again on the same commit costs nothing.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "goal_complete",
            "description": "Declare the whole goal done after verifying it on colony-main. Refused while any task is open or claimed, or when the configured verify command fails.",
            "inputSchema": { "type": "object", "properties": { "summary": { "type": "string" } }, "required": ["summary"] }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_accept_the_aliases_models_actually_send() {
        assert_eq!(arg(&json!({ "task_id": "T3" }), "id").unwrap(), "T3");
        assert_eq!(arg(&json!({ "message": "hi" }), "text").unwrap(), "hi");
        assert_eq!(
            arg(&json!({ "id": "T1", "note": "half done" }), "text").unwrap(),
            "half done"
        );
        assert_eq!(
            arg(&json!({ "text": " " }), "text").unwrap_err(),
            "`text` is required"
        );
    }

    #[test]
    fn verify_cache_hits_only_on_the_same_head() {
        let run = VerifyRun {
            head: "abc".into(),
            ok: true,
            code: Some(0),
            tail: String::new(),
            by: "w3".into(),
            at_ms: 0,
        };
        let cache = Some(run);
        assert!(cached_for(&cache, "abc").is_some());
        assert!(cached_for(&cache, "def").is_none());
        assert!(cached_for(&None, "abc").is_none());
    }

    #[test]
    fn complete_reply_ends_the_turn_and_asks_for_a_note_when_none_was_added() {
        let with_note = complete_reply("T3", true);
        assert!(with_note.contains("End your turn now"), "{with_note}");
        assert!(!with_note.contains("note_add"), "{with_note}");
        assert!(complete_reply("T3", false).contains("note_add"));
    }

    #[test]
    fn tool_list_offers_verify() {
        let tools = tool_list();
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"verify"), "{names:?}");
    }

    #[test]
    fn propose_accepts_flat_and_stringified_forms() {
        let flat = proposed_tasks(&json!({ "title": "a", "description": "d" })).unwrap();
        assert_eq!((flat[0].title.as_str(), flat[0].detail.as_str()), ("a", "d"));
        let stringified =
            proposed_tasks(&json!({ "tasks": "[{\"title\":\"b\",\"deps\":\"T1, T2\"}]" })).unwrap();
        assert_eq!(stringified[0].deps, vec!["T1", "T2"]);
        assert!(proposed_tasks(&json!({ "tasks": 3 })).is_err());
    }
}
