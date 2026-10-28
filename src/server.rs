//! Harness HTTP server (127.0.0.1 only): MCP endpoint, gate, dashboard.
//! The board lives once, in this process's memory, and every change is serialized here — which is why
//! claims are atomic.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::board::Board;
use crate::gate::Gate;
use crate::hub::Hub;
use crate::mcp;
use crate::supervisor::{BoardSignal, RunStatus, SupEvent};

const DASHBOARD: &str = include_str!("dashboard.html");

pub struct Verify {
    pub command: String,
    pub workspace: PathBuf,
}

pub struct Shared {
    board: Mutex<Board>,
    pub hub: Hub,
    pub gate: Arc<Gate>,
    pub status: Mutex<RunStatus>,
    pub verify: Option<Verify>,
    pub verify_lock: tokio::sync::Mutex<()>,
    sup_tx: mpsc::UnboundedSender<SupEvent>,
    tool_log: Mutex<Option<std::fs::File>>,
}

impl Shared {
    pub fn new(
        board: Board,
        hub: Hub,
        gate: Gate,
        verify: Option<Verify>,
        sup_tx: mpsc::UnboundedSender<SupEvent>,
        tool_log: Option<std::fs::File>,
    ) -> Self {
        Shared {
            board: Mutex::new(board),
            hub,
            gate: Arc::new(gate),
            status: Mutex::new(RunStatus::default()),
            verify,
            verify_lock: tokio::sync::Mutex::new(()),
            sup_tx,
            tool_log: Mutex::new(tool_log),
        }
    }

    /// Locks the board. Never held across an await. A poisoned lock (after a panic) is still used.
    pub fn board(&self) -> MutexGuard<'_, Board> {
        self.board.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn signal(&self, s: BoardSignal) {
        let _ = self.sup_tx.send(SupEvent::Board(s));
    }

    /// Tool-call audit log (`tools.jsonl`), for reconstructing who did what after a run.
    /// Also keeps the start of each result, so refusal reasons are visible without digging through
    /// raw worker logs.
    pub fn log_tool(&self, agent: &str, tool: &str, args: &Value, result: &str, is_error: bool) {
        if let Ok(mut guard) = self.tool_log.lock() {
            if let Some(f) = guard.as_mut() {
                let line = json!({
                    "at": chrono::Utc::now().timestamp_millis(),
                    "agent": agent,
                    "tool": tool,
                    "args": args,
                    "error": is_error,
                    "result": crate::board::clip(result, 400),
                });
                let _ = writeln!(f, "{line}");
            }
        }
    }
}

pub fn router(app: Arc<Shared>) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/api/state", get(state))
        .route("/mcp", get(no_stream).post(mcp_post).delete(no_stream))
        .route("/gate/hold", get(gate_hold))
        .with_state(app)
}

pub async fn serve(app: Arc<Shared>, port: u16) -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .context("failed to bind the server port")?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router(app)).await;
    });
    Ok((addr, handle))
}

async fn mcp_post(State(app): State<Arc<Shared>>, headers: HeaderMap, body: Bytes) -> Response {
    let agent = headers
        .get("x-colony-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    match mcp::handle(&app, agent.as_deref(), &body).await {
        Some(v) => Json(v).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// The server is stateless, so there is no server-to-client stream (GET) and no session teardown (DELETE).
async fn no_stream() -> StatusCode {
    StatusCode::METHOD_NOT_ALLOWED
}

async fn gate_hold(State(app): State<Arc<Shared>>, Query(q): Query<HashMap<String, String>>) -> Response {
    let agent = q.get("agent").cloned().unwrap_or_else(|| "?".into());
    let label = q.get("label").cloned().unwrap_or_default();
    let body = app.gate.clone().hold_body(agent, label);
    (
        [
            (header::CONTENT_TYPE, "text/plain"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn state(State(app): State<Arc<Shared>>) -> Json<Value> {
    let board = app.board().snapshot();
    let status = app
        .status
        .lock()
        .map(|s| serde_json::to_value(&*s).unwrap_or(Value::Null))
        .unwrap_or(Value::Null);
    Json(json!({
        "board": board,
        "status": status,
        "gate": { "slots": app.gate.slots, "holders": app.gate.holders() },
    }))
}

async fn dashboard() -> Html<&'static str> {
    Html(DASHBOARD)
}
