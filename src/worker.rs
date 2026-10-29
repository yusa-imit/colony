//! A worker is one `claude -p` process. Both stdin and stdout speak stream-json, so the process stays
//! alive and each turn is a message pushed into it. No process is spawned per turn, so context and the
//! prompt cache carry over.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::config::Config;
use crate::stream::parse_line;
use crate::supervisor::SupEvent;

/// MCP tool-call timeout (ms). Generous because goal_complete waits for the verify command.
const MCP_TOOL_TIMEOUT_MS: &str = "1800000";

/// Seconds a `colony inbox` hook may take. It answers from memory in milliseconds; this only bounds a stuck
/// harness.
const INBOX_HOOK_TIMEOUT_SECS: u64 = 5;

/// Variables a launching Claude Code session sets for itself and its children. Inherited by workers, they
/// loaded the terminal's inline plugin into every worker (`CLAUDE_CODE_PLUGIN_DIRS`), handed workers a socket
/// to the parent session (`CLAUDE_CODE_MESSAGING_*`) and set their effort (`CLAUDE_EFFORT`), all observed in
/// a real run. A deny list rather than an allow list, so authentication, proxies and toolchains still work.
const SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SSE_PORT",
];
const SESSION_VAR_PREFIXES: &[&str] = &[
    "CLAUDE_CODE_PLUGIN",
    "CLAUDE_CODE_MESSAGING",
    "CLAUDE_CODE_SESSION",
    "CLAUDE_CODE_CHILD",
];

pub fn inherited_session_var(name: &str) -> bool {
    SESSION_VARS.contains(&name) || SESSION_VAR_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// Ends the running turn at once (claude 2.1.312: the turn ends with `error_during_execution` within 50 ms).
pub fn interrupt_line() -> String {
    let line = json!({
        "type": "control_request",
        "request_id": format!("colony-interrupt-{}", uuid::Uuid::new_v4()),
        "request": { "subtype": "interrupt" },
    });
    format!("{line}\n")
}

pub const UNATTENDED_GIT_ENV: [(&str, &str); 4] = [
    ("GIT_EDITOR", "true"),
    ("GIT_SEQUENCE_EDITOR", "true"),
    ("GIT_MERGE_AUTOEDIT", "no"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

pub struct SpawnSpec<'a> {
    pub cfg: &'a Config,
    pub agent: &'a str,
    pub run_id: &'a str,
    pub cwd: &'a Path,
    /// The hub repository; writable from the sandbox so pushes work.
    pub hub_dir: &'a Path,
    /// The colony binary the inbox hook runs. `None` disables mid-turn delivery.
    pub colony_bin: Option<&'a Path>,
    pub session_id: &'a str,
    pub resume: bool,
    pub server_url: &'a str,
    pub system_prompt: &'a str,
    pub log_path: PathBuf,
}

/// Arguments for claude. The user's global settings (plugins, hooks, MCP servers) are off; only the target
/// repository's project settings apply.
pub fn claude_args(s: &SpawnSpec) -> Vec<String> {
    let mcp = json!({
        "mcpServers": {
            "colony": {
                "type": "http",
                "url": format!("{}/mcp", s.server_url),
                "headers": { "x-colony-agent": s.agent },
            }
        }
    });
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--replay-user-messages",
        "--strict-mcp-config",
        "--permission-prompts",
        "none",
        // Moves the per-worker cwd into the first user message so every worker's system prompt is identical
        // and the prompt cache is shared.
        "--exclude-dynamic-system-prompt-sections",
    ]
    .iter()
    .map(|x| x.to_string())
    .collect();
    a.extend(["--mcp-config".into(), mcp.to_string()]);
    a.extend(["--settings".into(), worker_settings(s).to_string()]);
    a.extend(["--setting-sources".into(), s.cfg.setting_sources.clone()]);
    a.extend(["--permission-mode".into(), s.cfg.permission_mode.clone()]);
    a.extend(["--append-system-prompt".into(), s.system_prompt.to_string()]);
    a.extend(["--name".into(), format!("colony-{}-{}", s.run_id, s.agent)]);
    if s.resume {
        a.extend(["--resume".into(), s.session_id.to_string()]);
    } else {
        a.extend(["--session-id".into(), s.session_id.to_string()]);
    }
    if let Some(m) = &s.cfg.model {
        a.extend(["--model".into(), m.clone()]);
    }
    if let Some(e) = &s.cfg.effort {
        a.extend(["--effort".into(), e.clone()]);
    }
    if let Some(b) = s.cfg.worker_budget_usd {
        a.extend(["--max-budget-usd".into(), b.to_string()]);
    }
    if !s.cfg.tools.is_empty() {
        a.extend(["--tools".into(), s.cfg.tools.join(",")]);
    }
    // Variadic flags get one argv entry per item, so rules containing spaces like `Bash(git *)` stay whole.
    // File writes are allowed inside the worker's own clone only (`//` marks an absolute path).
    a.push("--allowedTools".into());
    a.extend(s.cfg.allowed_tools.iter().cloned());
    let clone = s.cwd.to_string_lossy();
    a.extend([format!("Edit(/{clone}/**)"), format!("Write(/{clone}/**)")]);
    if !s.cfg.disallowed_tools.is_empty() {
        a.push("--disallowedTools".into());
        a.extend(s.cfg.disallowed_tools.iter().cloned());
    }
    a
}

/// `--settings` for a worker.
/// - No AI trailers: with these keys no Co-Authored-By is written (observed in real runs). If one slips
///   through, the clone's commit-msg hook strips it and the hub's pre-receive hook rejects it (hub.rs).
/// - Mid-turn delivery: after every tool call, `colony inbox` hands the model any direct mention or new note
///   it has not seen (verified on claude 2.1.312, also under `--setting-sources project,local`).
/// - Optional Bash sandbox, the closest local equivalent of Agensh's containers.
fn worker_settings(s: &SpawnSpec) -> Value {
    let mut v = json!({ "includeCoAuthoredBy": false, "attribution": { "commit": "", "pr": "" } });
    if let (true, Some(bin)) = (s.cfg.deliver_mid_turn, s.colony_bin) {
        let quoted = format!("'{}'", bin.to_string_lossy().replace('\'', "'\\''"));
        v["hooks"] = json!({ "PostToolUse": [{ "matcher": "*", "hooks": [{
            "type": "command",
            "command": format!("{quoted} inbox"),
            "timeout": INBOX_HOOK_TIMEOUT_SECS,
        }]}]});
    }
    if s.cfg.sandbox {
        let mut write = vec![s.hub_dir.to_string_lossy().into_owned()];
        write.extend(s.cfg.sandbox_allow_write.iter().cloned());
        let mut domains = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        domains.extend(s.cfg.sandbox_allowed_domains.iter().cloned());
        v["sandbox"] = json!({
            "enabled": true,
            "autoAllowBashIfSandboxed": true,
            "failIfUnavailable": true,
            "filesystem": { "allowWrite": write },
            "network": { "allowedDomains": domains },
        });
    }
    v
}

pub struct WorkerHandle {
    stdin: Option<ChildStdin>,
    kill: Option<oneshot::Sender<()>>,
}

impl WorkerHandle {
    pub async fn send(&mut self, text: &str) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(std::io::Error::other("stdin already closed"));
        };
        let line = json!({ "type": "user", "message": { "role": "user", "content": text } });
        stdin.write_all(format!("{line}\n").as_bytes()).await?;
        stdin.flush().await
    }

    /// Asks claude to end the running turn now. Used when the cost cap is reached.
    pub async fn interrupt(&mut self) -> std::io::Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Ok(());
        };
        stdin.write_all(interrupt_line().as_bytes()).await?;
        stdin.flush().await
    }

    /// Closes stdin. The in-flight turn finishes and the process exits on its own.
    pub fn close(&mut self) {
        self.stdin = None;
    }

    pub fn kill(&mut self) {
        self.stdin = None;
        if let Some(k) = self.kill.take() {
            let _ = k.send(());
        }
    }
}

/// Spawns the process and streams its output events to `tx`, then sends `SupEvent::Exited` when it ends.
/// `generation` increments on every restart, which lets the supervisor drop late events from a dead process.
pub fn spawn(
    spec: &SpawnSpec,
    idx: usize,
    generation: u32,
    tx: mpsc::UnboundedSender<SupEvent>,
) -> Result<WorkerHandle> {
    let self_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let path = match (self_dir, std::env::var_os("PATH")) {
        (Some(dir), Some(p)) => {
            let mut parts = vec![dir];
            parts.extend(std::env::split_paths(&p));
            std::env::join_paths(parts).ok()
        }
        _ => None,
    };
    let log_std = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spec.log_path.with_extension("stderr.log"))
        .context("failed to open the stderr log file")?;
    let mut cmd = Command::new(&spec.cfg.claude_bin);
    cmd.args(claude_args(spec))
        .current_dir(spec.cwd)
        .env("COLONY_URL", spec.server_url)
        .env("COLONY_AGENT", spec.agent)
        .env("MCP_TOOL_TIMEOUT", MCP_TOOL_TIMEOUT_MS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log_std))
        .kill_on_drop(true);
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    for (name, _) in std::env::vars_os() {
        if inherited_session_var(&name.to_string_lossy()) {
            cmd.env_remove(&name);
        }
    }
    // Unattended git: never open an editor or a credential prompt. These are set here because a worker
    // that prefixes them itself (`GIT_EDITOR=true git …`) no longer matches the `Bash(git *)` allow rule
    // and gets denied.
    for (k, v) in UNATTENDED_GIT_ENV {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to run `{}`", spec.cfg.claude_bin))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("child has no stdout")?;
    let log_path = spec.log_path.clone();

    let reader_tx = tx.clone();
    let reader = tokio::spawn(async move {
        let mut log = match tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .await
        {
            Ok(f) => Some(BufWriter::new(f)),
            Err(_) => None,
        };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(log) = log.as_mut() {
                let _ = log.write_all(line.as_bytes()).await;
                let _ = log.write_all(b"\n").await;
                let _ = log.flush().await;
            }
            if let Some(ev) = parse_line(&line) {
                let _ = reader_tx.send(SupEvent::Stream { idx, generation, ev });
            }
        }
    });

    let (kill_tx, kill_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let code = tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()),
            _ = kill_rx => {
                let _ = child.kill().await;
                None
            }
        };
        // Report the exit only after all output is read, so the final result arrives before the exit.
        let _ = reader.await;
        let _ = tx.send(SupEvent::Exited {
            idx,
            generation,
            code,
        });
    });

    Ok(WorkerHandle {
        stdin,
        kill: Some(kill_tx),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig, Overrides};

    fn cfg() -> Config {
        let mut cfg = Config::resolve(
            "g".into(),
            "/r".into(),
            FileConfig::default(),
            Overrides::default(),
        )
        .unwrap();
        cfg.model = Some("sonnet".into());
        cfg
    }

    fn spec(cfg: &Config) -> SpawnSpec<'_> {
        SpawnSpec {
            cfg,
            agent: "w03",
            run_id: "r1",
            cwd: Path::new("/runs/x/ws/w03"),
            hub_dir: Path::new("/runs/x/hub.git"),
            colony_bin: Some(Path::new("/opt/my tools/colony")),
            session_id: "00000000-0000-4000-8000-000000000000",
            resume: false,
            server_url: "http://127.0.0.1:9",
            system_prompt: "proto",
            log_path: PathBuf::from("/tmp/w03.jsonl"),
        }
    }

    fn settings(a: &[String]) -> serde_json::Value {
        let i = a.iter().position(|x| x == "--settings").unwrap();
        serde_json::from_str(&a[i + 1]).unwrap()
    }

    #[test]
    fn args_keep_tool_rules_whole_and_isolate_user_config() {
        let cfg = cfg();
        let spec = spec(&cfg);
        let a = claude_args(&spec);
        assert!(
            a.contains(&"Bash(git *)".to_string()),
            "rules with spaces stay single args"
        );
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--setting-sources" && w[1] == "project,local"));
        assert!(a.windows(2).any(|w| w[0] == "--session-id"));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        let mcp = a
            .iter()
            .position(|x| x == "--mcp-config")
            .map(|i| &a[i + 1])
            .unwrap();
        assert!(mcp.contains("\"x-colony-agent\":\"w03\""), "{mcp}");
        assert!(mcp.contains("http://127.0.0.1:9/mcp"), "{mcp}");
        let resumed = claude_args(&SpawnSpec { resume: true, ..spec });
        assert!(resumed.windows(2).any(|w| w[0] == "--resume"));
    }

    #[test]
    fn args_limit_tools_and_scope_file_writes_to_the_clone() {
        let cfg = cfg();
        let a = claude_args(&spec(&cfg));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--tools" && w[1] == "Bash,Read,Edit,Write,Glob,Grep"));
        for rule in ["Edit(//runs/x/ws/w03/**)", "Write(//runs/x/ws/w03/**)"] {
            assert!(a.contains(&rule.to_string()), "missing {rule}: {a:?}");
        }
        assert!(!a.contains(&"Write".to_string()) && !a.contains(&"Edit".to_string()));
    }

    #[test]
    fn settings_carry_attribution_off_and_the_inbox_hook() {
        let cfg = cfg();
        let s = settings(&claude_args(&spec(&cfg)));
        assert_eq!(s["includeCoAuthoredBy"], false);
        assert_eq!(s["attribution"]["commit"], "");
        let hook = &s["hooks"]["PostToolUse"][0];
        assert_eq!(hook["matcher"], "*");
        assert_eq!(hook["hooks"][0]["command"], "'/opt/my tools/colony' inbox");
        assert!(s.get("sandbox").is_none(), "sandbox is opt-in");

        let mut off = cfg.clone();
        off.deliver_mid_turn = false;
        assert!(settings(&claude_args(&spec(&off))).get("hooks").is_none());
    }

    #[test]
    fn sandbox_settings_allow_the_hub_and_localhost() {
        let mut cfg = cfg();
        cfg.sandbox = true;
        cfg.sandbox_allowed_domains = vec!["registry.npmjs.org".into()];
        cfg.sandbox_allow_write = vec!["/Users/me/Library/pnpm".into()];
        let s = settings(&claude_args(&spec(&cfg)));
        let sb = &s["sandbox"];
        assert_eq!(sb["enabled"], true);
        assert_eq!(sb["autoAllowBashIfSandboxed"], true);
        assert_eq!(sb["failIfUnavailable"], true);
        assert_eq!(
            sb["filesystem"]["allowWrite"],
            serde_json::json!(["/runs/x/hub.git", "/Users/me/Library/pnpm"])
        );
        assert_eq!(
            sb["network"]["allowedDomains"],
            serde_json::json!(["127.0.0.1", "localhost", "registry.npmjs.org"])
        );
    }

    #[test]
    fn strips_the_launching_sessions_variables_only() {
        for leak in [
            "CLAUDECODE",
            "CLAUDE_EFFORT",
            "CLAUDE_PID",
            "CLAUDE_CODE_PLUGIN_DIRS",
            "CLAUDE_CODE_MESSAGING_SOCKET",
            "CLAUDE_CODE_MESSAGING_TOKEN",
            "CLAUDE_CODE_SESSION_ID",
            "CLAUDE_CODE_SESSION_ATTENDED",
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_EXECPATH",
            "CLAUDE_CODE_SSE_PORT",
        ] {
            assert!(inherited_session_var(leak), "{leak}");
        }
        for keep in [
            "PATH",
            "HOME",
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "HTTPS_PROXY",
        ] {
            assert!(!inherited_session_var(keep), "{keep}");
        }
    }

    #[test]
    fn interrupt_is_a_control_request() {
        let v: serde_json::Value = serde_json::from_str(interrupt_line().trim()).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["request"]["subtype"], "interrupt");
        assert!(v["request_id"].as_str().is_some_and(|s| !s.is_empty()));
    }
}
