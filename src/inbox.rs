//! `colony inbox`, run by every worker's PostToolUse hook. It fetches what the worker has not been shown yet
//! (direct mentions, other workers' new notes) and prints it as hook output, so it reaches the model right
//! after the tool result. Agensh delivers direct messages and shared-context entries the same way, appended
//! to tool results. Without it a worker coding for minutes was deaf: a peer's "I'll take that helper"
//! arrived after the duplicate was merged (observed in a real run).
//!
//! Outside a colony, or when the harness does not answer, it prints nothing. It never fails the tool call.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::json;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const IO_TIMEOUT: Duration = Duration::from_secs(3);

/// Hook output carrying `text` to the model, or `None` when there is nothing to say.
pub fn hook_output(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let v = json!({
        "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": text }
    });
    Some(v.to_string())
}

/// Entry point of `colony inbox`. Always exits 0.
pub fn run_inbox_hook() -> i32 {
    // The hook input (the tool call and its result) is not needed, but it is drained so claude never blocks
    // writing a large tool result into a pipe nobody reads.
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
    let (Ok(url), Ok(agent)) = (std::env::var("COLONY_URL"), std::env::var("COLONY_AGENT")) else {
        return 0;
    };
    if let Some(out) = fetch(&url, &agent).ok().as_deref().and_then(hook_output) {
        println!("{out}");
    }
    0
}

fn fetch(url: &str, agent: &str) -> std::io::Result<String> {
    let host = url.trim_start_matches("http://").trim_end_matches('/');
    let addr = host
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| std::io::Error::other("no address"))?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let req = format!(
        "GET /inbox?agent={} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n",
        crate::gate::encode(agent)
    );
    stream.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp)?;
    let resp = String::from_utf8_lossy(&resp);
    let (head, body) = resp
        .split_once("\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("malformed response"))?;
    if !head.starts_with("HTTP/1.1 200") {
        return Err(std::io::Error::other(
            head.lines().next().unwrap_or("").to_string(),
        ));
    }
    Ok(body.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_output_wraps_text_as_additional_context() {
        let out = hook_output("[colony] w1 → you: hi\n").unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "[colony] w1 → you: hi"
        );
    }

    #[test]
    fn hook_output_is_silent_when_nothing_is_pending() {
        assert_eq!(hook_output(""), None);
        assert_eq!(hook_output("  \n"), None);
    }

    #[test]
    fn unreachable_harness_yields_an_error_not_a_hang() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        assert!(fetch(&format!("http://127.0.0.1:{port}"), "w1").is_err());
    }
}
