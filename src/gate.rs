//! Limits how many heavy commands (dependency installs, builds, full test runs) run at once.
//!
//! Workers wrap such commands as `colony gate -- <command>`. The client holds a connection open to
//! `/gate/hold`, runs the command once it receives `ok`, then closes the connection. The server reclaims
//! the slot the moment the connection drops, whether the client exited cleanly or was killed, so a worker
//! Bash call that dies on a timeout cannot leak a slot.

use std::convert::Infallible;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use futures_util::stream;
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Interval between keepalive bytes that probe the connection. A write to a dropped socket frees the slot.
const KEEPALIVE: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize)]
pub struct Holder {
    pub agent: String,
    pub label: String,
    pub since_ms: i64,
}

#[derive(Debug)]
pub struct Gate {
    sem: Arc<Semaphore>,
    pub slots: usize,
    holders: Arc<Mutex<Vec<(u64, Holder)>>>,
    next_id: Mutex<u64>,
}

/// A held slot. Dropping it releases both the permit and the dashboard entry.
pub struct Lease {
    _permit: OwnedSemaphorePermit,
    id: u64,
    holders: Arc<Mutex<Vec<(u64, Holder)>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Ok(mut h) = self.holders.lock() {
            h.retain(|(id, _)| *id != self.id);
        }
    }
}

impl Gate {
    pub fn new(slots: usize) -> Self {
        Gate {
            sem: Arc::new(Semaphore::new(slots)),
            slots,
            holders: Arc::new(Mutex::new(Vec::new())),
            next_id: Mutex::new(0),
        }
    }

    pub async fn acquire(&self, agent: &str, label: &str) -> Lease {
        let permit = self
            .sem
            .clone()
            .acquire_owned()
            .await
            .expect("gate semaphore is never closed");
        let id = {
            let mut n = self.next_id.lock().expect("gate id lock");
            *n += 1;
            *n
        };
        let holder = Holder {
            agent: agent.to_string(),
            label: label.to_string(),
            since_ms: chrono::Utc::now().timestamp_millis(),
        };
        self.holders.lock().expect("gate holders lock").push((id, holder));
        Lease {
            _permit: permit,
            id,
            holders: self.holders.clone(),
        }
    }

    pub fn holders(&self) -> Vec<Holder> {
        self.holders
            .lock()
            .map(|h| h.iter().map(|(_, x)| x.clone()).collect())
            .unwrap_or_default()
    }

    /// Response body for `/gate/hold`: `ok\n` once a slot is acquired, then keepalives until the connection
    /// drops.
    pub fn hold_body(self: Arc<Self>, agent: String, label: String) -> Body {
        enum St {
            Waiting(Arc<Gate>, String, String),
            Holding(Lease),
        }
        let s = stream::unfold(St::Waiting(self, agent, label), |st| async move {
            match st {
                St::Waiting(gate, agent, label) => {
                    let lease = gate.acquire(&agent, &label).await;
                    Some((
                        Ok::<_, Infallible>(Bytes::from_static(b"ok\n")),
                        St::Holding(lease),
                    ))
                }
                St::Holding(lease) => {
                    tokio::time::sleep(KEEPALIVE).await;
                    Some((Ok(Bytes::from_static(b"\n")), St::Holding(lease)))
                }
            }
        });
        Body::from_stream(s)
    }
}

/// `colony gate -- <command>`. Outside the harness (no `COLONY_URL`) it just runs the command.
pub fn run_gated(cmd: &[String]) -> i32 {
    if cmd.is_empty() {
        eprintln!("colony gate: no command given. Example: colony gate -- pnpm test");
        return 2;
    }
    let lease = match std::env::var("COLONY_URL") {
        Ok(url) => {
            let agent = std::env::var("COLONY_AGENT").unwrap_or_else(|_| "?".into());
            let started = Instant::now();
            match hold(&url, &agent, &cmd.join(" ")) {
                Ok(stream) => {
                    let waited = started.elapsed();
                    if waited > Duration::from_secs(1) {
                        eprintln!("[colony gate] waited {}s for a slot", waited.as_secs());
                    }
                    Some(stream)
                }
                Err(e) => {
                    eprintln!("[colony gate] could not reach the gate ({e}); running without a slot");
                    None
                }
            }
        }
        Err(_) => None,
    };
    let status = if cmd.len() == 1 {
        Command::new("sh").arg("-c").arg(&cmd[0]).status()
    } else {
        Command::new(&cmd[0]).args(&cmd[1..]).status()
    };
    drop(lease);
    match status {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("colony gate: failed to run `{}`: {e}", cmd[0]);
            127
        }
    }
}

/// Blocks until a slot is acquired. The slot stays held for as long as the returned connection is open.
fn hold(url: &str, agent: &str, label: &str) -> std::io::Result<TcpStream> {
    let addr = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(addr)?;
    let req = format!(
        "GET /gate/hold?agent={}&label={} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n",
        encode(agent),
        encode(&crate::board::clip(label, 80)),
    );
    stream.write_all(req.as_bytes())?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(std::io::Error::other(
                "the gate closed the connection before granting a slot",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(head_end) = find(&buf, b"\r\n\r\n") {
            if !buf.starts_with(b"HTTP/1.1 200") {
                return Err(std::io::Error::other(
                    String::from_utf8_lossy(&buf[..head_end]).into_owned(),
                ));
            }
            if find(&buf[head_end..], b"ok\n").is_some() {
                return Ok(stream);
            }
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lease_drop_frees_slot_and_holder() {
        let gate = Arc::new(Gate::new(1));
        let lease = gate.acquire("w1", "pnpm test").await;
        assert_eq!(gate.holders().len(), 1);
        let g2 = gate.clone();
        let waiter = tokio::spawn(async move { g2.acquire("w2", "build").await });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "second acquire must wait while the only slot is held"
        );
        drop(lease);
        let _l2 = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        let holders = gate.holders();
        assert_eq!(holders.len(), 1);
        assert_eq!(holders[0].agent, "w2");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("pnpm -w test"), "pnpm%20-w%20test");
        assert_eq!(encode("w01"), "w01");
    }
}
