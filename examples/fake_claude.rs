//! A fake `claude` for tests. It speaks the same stream-json as the real one and calls the real MCP
//! endpoint over HTTP. Instead of an LLM it follows a fixed script: whoever claims the planning task T1
//! splits the goal into three tasks; everyone claims a task → commits a file → pushes to the hub
//! (rebasing when rejected) → task_complete, and tries goal_complete when there is nothing left to do.
//!
//! With `FAKE_LIMIT_AGENT=w2`, that worker's first turn ends on a usage limit (429).

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

struct Ctx {
    addr: String,
    agent: String,
    session: String,
    turns: u32,
    limited_once: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("0.0.0 (fake claude)");
        return;
    }
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let mcp: Value = serde_json::from_str(&flag("--mcp-config").expect("--mcp-config")).unwrap();
    let server = &mcp["mcpServers"]["colony"];
    let url = server["url"].as_str().unwrap();
    let mut ctx = Ctx {
        addr: url
            .trim_start_matches("http://")
            .trim_end_matches("/mcp")
            .to_string(),
        agent: server["headers"]["x-colony-agent"].as_str().unwrap().to_string(),
        session: flag("--session-id")
            .or_else(|| flag("--resume"))
            .unwrap_or_default(),
        turns: 0,
        limited_once: false,
    };
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let v: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"] != "user" {
            continue;
        }
        emit(json!({ "type": "system", "subtype": "init", "session_id": ctx.session, "model": "fake" }));
        turn(&mut ctx);
    }
}

fn emit(v: Value) {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{v}").unwrap();
    out.flush().unwrap();
}

fn turn(ctx: &mut Ctx) {
    ctx.turns += 1;
    let cost = f64::from(ctx.turns) * 0.01;
    if !ctx.limited_once && std::env::var("FAKE_LIMIT_AGENT").ok().as_deref() == Some(ctx.agent.as_str()) {
        ctx.limited_once = true;
        let resets = chrono_now_secs() + 2;
        emit(
            json!({ "type": "rate_limit_event", "rate_limit_info": { "status": "rejected", "resetsAt": resets } }),
        );
        emit(
            json!({ "type": "result", "subtype": "error_during_execution", "is_error": true,
                     "api_error_status": 429, "total_cost_usd": cost, "result": "rate limited" }),
        );
        return;
    }

    call(ctx, "board_view", json!({}));
    // Only the worker that claims the planning task T1 plans. The dep is given by title to exercise the
    // lenient input handling.
    if !call(ctx, "task_claim", json!({ "id": "T1", "plan": "split the goal" })).1 {
        let (text, err) = call(
            ctx,
            "task_propose",
            json!({ "tasks": [
                { "title": "write alpha file" },
                { "title": "write beta file", "description": "second" },
                { "title": "write gamma file", "deps": ["write alpha file"] }
            ]}),
        );
        assert!(!err, "planning refused: {text}");
        let (text, err) = call(
            ctx,
            "task_complete",
            json!({ "id": "T1", "summary": "planned 3 tasks" }),
        );
        assert!(!err, "completing the plan refused: {text}");
    }
    let mut claimed = None;
    for id in ["T2", "T3", "T4"] {
        let (_, err) = call(ctx, "task_claim", json!({ "id": id, "plan": "write the file" }));
        if !err {
            claimed = Some(id);
            break;
        }
    }
    let cwd = std::env::current_dir().unwrap();
    match claimed {
        Some(id) => {
            hold_gate(ctx);
            std::fs::write(cwd.join(format!("{id}.txt")), format!("{id} by {}\n", ctx.agent)).unwrap();
            git(&cwd, &["add", "."]);
            git(&cwd, &["commit", "-q", "-m", &format!("{id} by {}", ctx.agent)]);
            let mut pushed = false;
            for _ in 0..20 {
                git(&cwd, &["pull", "-q", "--rebase", "origin", "colony-main"]);
                if git(&cwd, &["push", "-q", "origin", "HEAD:colony-main"]) {
                    pushed = true;
                    break;
                }
            }
            assert!(pushed, "push kept failing");
            let sha = String::from_utf8(
                Command::new("git")
                    .args(["rev-parse", "HEAD"])
                    .current_dir(&cwd)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap();
            let (text, err) = call(
                ctx,
                "task_complete",
                json!({ "id": id, "summary": "wrote it", "commit": sha.trim() }),
            );
            assert!(!err, "task_complete refused: {text}");
            call(
                ctx,
                "note_add",
                json!({ "title": format!("{id} written"), "body": "plain text file" }),
            );
        }
        None => {
            call(
                ctx,
                "goal_complete",
                json!({ "summary": "all three files are on colony-main" }),
            );
        }
    }
    emit(json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": "turn done" }] } }));
    emit(
        json!({ "type": "result", "subtype": "success", "is_error": false, "total_cost_usd": cost, "result": "ok" }),
    );
}

fn call(ctx: &Ctx, tool: &str, args: Value) -> (String, bool) {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                       "params": { "name": tool, "arguments": args } })
    .to_string();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: {a}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n\
         x-colony-agent: {agent}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        a = ctx.addr,
        agent = ctx.agent,
        len = body.len()
    );
    let mut s = TcpStream::connect(&ctx.addr).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = String::new();
    s.read_to_string(&mut resp).unwrap();
    let body = resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let v: Value = serde_json::from_str(body).unwrap_or_else(|e| panic!("bad response {resp}: {e}"));
    let r = &v["result"];
    (
        r["content"][0]["text"].as_str().unwrap_or("").to_string(),
        r["isError"].as_bool().unwrap_or(true),
    )
}

/// Takes and releases a gate slot, to exercise the server's `/gate/hold` path.
fn hold_gate(ctx: &Ctx) {
    let mut s = TcpStream::connect(&ctx.addr).unwrap();
    let req = format!(
        "GET /gate/hold?agent={}&label=fake HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        ctx.agent, ctx.addr
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    while !String::from_utf8_lossy(&buf).contains("ok\n") {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "gate closed early");
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn git(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn chrono_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
