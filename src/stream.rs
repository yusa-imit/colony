//! Turns one line of `claude -p --output-format stream-json` into an event for the supervisor. No IO.
//!
//! Shapes were measured on claude 2.1.312: each turn emits `system/init` → `assistant`… →
//! `rate_limit_event` → `result`. `result.total_cost_usd` is cumulative within a process.

use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Init {
        session_id: String,
        model: String,
    },
    /// One line on what the worker is doing now (a tool call or text).
    Activity(String),
    RateLimit(RateLimitInfo),
    TurnEnd(TurnResult),
}

#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub struct RateLimitInfo {
    /// `allowed`, `allowed_warning`, `rejected`, …; unknown values are passed through as-is.
    pub status: String,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    pub five_hour: Option<f64>,
    pub seven_day: Option<f64>,
}

impl RateLimitInfo {
    pub fn is_rejected(&self) -> bool {
        self.status == "rejected"
    }

    pub fn is_warning(&self) -> bool {
        self.status.contains("warning")
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TurnResult {
    pub is_error: bool,
    pub subtype: String,
    pub api_error_status: Option<i64>,
    /// Cumulative cost of this process.
    pub total_cost_usd: f64,
    pub text: String,
    /// The turn ended on a usage limit or overload. The supervisor slows everyone down and requeues the
    /// same work.
    pub limited: bool,
}

const LIMIT_MARKERS: [&str; 6] = [
    "rate limit",
    "rate_limit",
    "usage limit",
    "session limit",
    "overloaded",
    "too many requests",
];

pub fn parse_line(line: &str) -> Option<StreamEvent> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("type")?.as_str()? {
        "system" if v.get("subtype").and_then(Value::as_str) == Some("init") => Some(StreamEvent::Init {
            session_id: str_of(&v, "session_id"),
            model: str_of(&v, "model"),
        }),
        "assistant" => activity(&v).map(StreamEvent::Activity),
        "rate_limit_event" => {
            let info = v.get("rate_limit_info")?;
            let window = |name: &str| {
                info.get("unifiedWindows")
                    .and_then(|w| w.get(name))
                    .and_then(|w| w.get("utilization"))
                    .and_then(Value::as_f64)
            };
            Some(StreamEvent::RateLimit(RateLimitInfo {
                status: str_of(info, "status"),
                resets_at: info.get("resetsAt").and_then(Value::as_i64),
                five_hour: window("five_hour"),
                seven_day: window("seven_day"),
            }))
        }
        "result" => {
            let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let api_error_status = v.get("api_error_status").and_then(Value::as_i64);
            let text = v
                .get("result")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| v.get("errors").map(|e| e.to_string()))
                .unwrap_or_default();
            let lower = text.to_lowercase();
            let limited = matches!(api_error_status, Some(429 | 529))
                || (is_error && LIMIT_MARKERS.iter().any(|m| lower.contains(m)));
            Some(StreamEvent::TurnEnd(TurnResult {
                is_error,
                subtype: str_of(&v, "subtype"),
                api_error_status,
                total_cost_usd: v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0),
                text,
                limited,
            }))
        }
        _ => None,
    }
}

fn str_of(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The last tool call in an assistant message (or its text, if none), as one line.
fn activity(v: &Value) -> Option<String> {
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut last_text = None;
    let mut last_tool = None;
    for c in content {
        match c.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let name = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                let input = c.get("input").cloned().unwrap_or(Value::Null);
                let arg = ["command", "file_path", "pattern", "path", "id", "text", "title"]
                    .iter()
                    .find_map(|k| input.get(*k).and_then(Value::as_str))
                    .unwrap_or("");
                let name = name.strip_prefix("mcp__colony__").unwrap_or(name);
                last_tool = Some(crate::board::clip(&format!("{name} {arg}"), 120));
            }
            Some("text") => {
                let t = c.get("text").and_then(Value::as_str).unwrap_or("").trim();
                if !t.is_empty() {
                    last_text = Some(crate::board::clip(t, 120));
                }
            }
            _ => {}
        }
    }
    last_tool.or(last_text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_init() {
        let ev =
            parse_line(r#"{"type":"system","subtype":"init","session_id":"s1","model":"claude-haiku-5-5"}"#);
        assert_eq!(
            ev,
            Some(StreamEvent::Init {
                session_id: "s1".into(),
                model: "claude-haiku-5-5".into()
            })
        );
    }

    #[test]
    fn parses_rate_limit_event_as_recorded() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1793152800,"rateLimitType":"five_hour","unifiedWindows":{"five_hour":{"utilization":0.09,"resetsAt":1793152800,"status":"allowed"},"seven_day":{"utilization":0.21,"resetsAt":1793660400,"status":"allowed"}}},"uuid":"u","session_id":"s"}"#;
        let Some(StreamEvent::RateLimit(info)) = parse_line(line) else {
            panic!("not a rate limit event")
        };
        assert_eq!(info.status, "allowed");
        assert_eq!(info.resets_at, Some(1793152800));
        assert_eq!(info.five_hour, Some(0.09));
        assert_eq!(info.seven_day, Some(0.21));
        assert!(!info.is_rejected());
    }

    #[test]
    fn parses_successful_result() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"result":"pong","total_cost_usd":0.0007}"#;
        let Some(StreamEvent::TurnEnd(r)) = parse_line(line) else {
            panic!("not a result")
        };
        assert!(!r.is_error && !r.limited);
        assert_eq!(r.total_cost_usd, 0.0007);
        assert_eq!(r.text, "pong");
    }

    #[test]
    fn detects_limited_turns() {
        let by_status = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":429,"total_cost_usd":0.1}"#;
        let by_text = r#"{"type":"result","subtype":"success","is_error":true,"result":"Claude AI usage limit reached|1793152800"}"#;
        let plain_error =
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"max turns"}"#;
        for (line, want) in [(by_status, true), (by_text, true), (plain_error, false)] {
            let Some(StreamEvent::TurnEnd(r)) = parse_line(line) else {
                panic!("not a result")
            };
            assert_eq!(r.limited, want, "{line}");
        }
    }

    #[test]
    fn summarizes_tool_use_and_strips_colony_prefix() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"let me check"},{"type":"tool_use","name":"mcp__colony__task_claim","input":{"id":"T3","plan":"x"}}]}}"#;
        assert_eq!(
            parse_line(line),
            Some(StreamEvent::Activity("task_claim T3".into()))
        );
        let bash = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git push origin HEAD:colony-main"}}]}}"#;
        assert_eq!(
            parse_line(bash),
            Some(StreamEvent::Activity(
                "Bash git push origin HEAD:colony-main".into()
            ))
        );
    }

    #[test]
    fn ignores_noise() {
        assert_eq!(parse_line("not json"), None);
        assert_eq!(parse_line(r#"{"type":"user","message":{}}"#), None);
    }
}
