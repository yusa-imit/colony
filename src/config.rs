//! Run configuration. Layered as defaults ← the target repo's `colony.toml` ← CLI flags.
//!
//! The harness only knows mechanism. Repo-specific policy — install and verify commands, allowed
//! tools — lives in `colony.toml`.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

pub const CONFIG_FILE: &str = "colony.toml";

/// Built-in tools a worker gets (`--tools`). A real run used only these; the full surface (32 tools, including
/// push notifications, remote triggers and web access) acted outside the hub and added to every prompt. With
/// `ToolSearch` absent, the colony MCP tools load up front instead of costing a search per session.
pub const DEFAULT_TOOLS: &[&str] = &["Bash", "Read", "Edit", "Write", "Glob", "Grep"];

/// Tools an unattended worker may use without asking. Everything else is denied via
/// `--permission-prompts none`. There is no bare `Write`/`Edit`: it allowed writes anywhere (observed: 30
/// writes to /tmp). Each worker gets rules scoped to its own clone instead (worker.rs).
pub const DEFAULT_ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Glob",
    "Grep",
    "TodoWrite",
    "mcp__colony",
    "Bash(colony gate *)",
    "Bash(git *)",
    // The protocol forbids env prefixes, yet models still add this one during rebase conflicts (observed
    // in real runs). Harmless: the harness already sets the same value.
    "Bash(GIT_EDITOR=true git *)",
    "Bash(npm *)",
    "Bash(npx *)",
    "Bash(pnpm *)",
    "Bash(yarn *)",
    "Bash(node *)",
    "Bash(cargo *)",
    "Bash(ls *)",
    "Bash(cat *)",
    "Bash(head *)",
    "Bash(tail *)",
    "Bash(wc *)",
    "Bash(find *)",
    "Bash(grep *)",
    "Bash(rg *)",
    "Bash(diff *)",
    "Bash(mkdir *)",
    "Bash(mv *)",
    "Bash(cp *)",
    // Denied in a real run 19, 2 and 1 times: `cd <dir> && …`, `sed -i`, local binaries.
    "Bash(cd *)",
    "Bash(sed *)",
    "Bash(./node_modules/.bin/*)",
];

/// Tools blocked in unattended runs. Subagents multiply concurrency outside the harness's control,
/// and there is no human to answer questions.
pub const DEFAULT_DISALLOWED_TOOLS: &[&str] = &["Agent", "AskUserQuestion"];

/// `colony.toml`. Every field is optional.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub workers: Option<usize>,
    pub max_active: Option<usize>,
    pub ramp_secs: Option<u64>,
    pub hours: Option<f64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub tools: Option<Vec<String>>,
    pub allowed_tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
    pub setting_sources: Option<String>,
    pub setup: Option<String>,
    pub verify: Option<String>,
    pub gate_slots: Option<usize>,
    pub max_cost_usd: Option<f64>,
    pub worker_budget_usd: Option<f64>,
    pub base: Option<String>,
    pub idle_secs: Option<u64>,
    pub grace_secs: Option<u64>,
    pub fresh_session: Option<bool>,
    pub deliver_mid_turn: Option<bool>,
    pub keep_workspaces: Option<bool>,
    pub sandbox: Option<bool>,
    pub sandbox_allowed_domains: Option<Vec<String>>,
    pub sandbox_allow_write: Option<Vec<String>>,
    pub claude_bin: Option<String>,
    pub state_dir: Option<String>,
}

/// Overrides from the CLI. `None` falls back to the file, then the default.
#[derive(Debug, Default, Clone)]
pub struct Overrides {
    pub workers: Option<usize>,
    pub max_active: Option<usize>,
    pub ramp_secs: Option<u64>,
    pub hours: Option<f64>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    pub setup: Option<String>,
    pub verify: Option<String>,
    pub gate_slots: Option<usize>,
    pub max_cost_usd: Option<f64>,
    pub base: Option<String>,
    pub idle_secs: Option<u64>,
    pub state_dir: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub goal: String,
    pub repo: PathBuf,
    pub workers: usize,
    /// Maximum workers taking a turn at once. The supervisor runs below this after hitting usage limits.
    pub max_active: usize,
    pub ramp_secs: u64,
    pub hours: f64,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: String,
    /// Built-in tool set (`--tools`).
    pub tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub setting_sources: String,
    pub setup: Option<String>,
    pub verify: Option<String>,
    pub gate_slots: usize,
    pub max_cost_usd: Option<f64>,
    pub worker_budget_usd: Option<f64>,
    pub base: String,
    pub idle_secs: u64,
    pub grace_secs: u64,
    /// Restart a worker in a new session after each task it completes. Carrying earlier tasks' transcripts
    /// was about a quarter of a real run's cost.
    pub fresh_session: bool,
    /// Deliver direct mentions and new notes after each tool call (PostToolUse hook → `colony inbox`).
    pub deliver_mid_turn: bool,
    /// Keep `ws/` after a completed run. By default it is removed (a real run left 2.2 GB).
    pub keep_workspaces: bool,
    /// Run Bash in Claude Code's sandbox: commands are auto-allowed and writes are confined to the clone,
    /// the hub, temp dirs and `sandbox_allow_write`. Network is limited to localhost (the gate) and
    /// `sandbox_allowed_domains`.
    pub sandbox: bool,
    pub sandbox_allowed_domains: Vec<String>,
    pub sandbox_allow_write: Vec<String>,
    pub claude_bin: String,
    pub state_dir: PathBuf,
    pub port: u16,
}

impl Config {
    pub fn resolve(goal: String, repo: PathBuf, file: FileConfig, o: Overrides) -> Result<Config> {
        let workers = o.workers.or(file.workers).unwrap_or(8);
        let max_active = o.max_active.or(file.max_active).unwrap_or(workers.min(8));
        let state_dir = o
            .state_dir
            .or(file.state_dir)
            .unwrap_or_else(|| "~/.colony/runs".into());
        let cfg = Config {
            goal,
            repo,
            workers,
            max_active,
            ramp_secs: o.ramp_secs.or(file.ramp_secs).unwrap_or(15),
            hours: o.hours.or(file.hours).unwrap_or(2.0),
            model: o.model.or(file.model),
            effort: o.effort.or(file.effort),
            permission_mode: o
                .permission_mode
                .or(file.permission_mode)
                .unwrap_or_else(|| "acceptEdits".into()),
            tools: file
                .tools
                .unwrap_or_else(|| DEFAULT_TOOLS.iter().map(|s| s.to_string()).collect()),
            allowed_tools: file
                .allowed_tools
                .unwrap_or_else(|| DEFAULT_ALLOWED_TOOLS.iter().map(|s| s.to_string()).collect()),
            disallowed_tools: file
                .disallowed_tools
                .unwrap_or_else(|| DEFAULT_DISALLOWED_TOOLS.iter().map(|s| s.to_string()).collect()),
            setting_sources: file.setting_sources.unwrap_or_else(|| "project,local".into()),
            setup: o.setup.or(file.setup).filter(|s| !s.trim().is_empty()),
            verify: o.verify.or(file.verify).filter(|s| !s.trim().is_empty()),
            gate_slots: o.gate_slots.or(file.gate_slots).unwrap_or(4),
            max_cost_usd: o.max_cost_usd.or(file.max_cost_usd),
            worker_budget_usd: file.worker_budget_usd,
            base: o.base.or(file.base).unwrap_or_else(|| "HEAD".into()),
            // Agensh's idle detector waits 10 minutes; checking idle workers every 2 minutes mostly bought
            // turns that found nothing to do.
            idle_secs: o.idle_secs.or(file.idle_secs).unwrap_or(600),
            grace_secs: file.grace_secs.unwrap_or(300),
            fresh_session: file.fresh_session.unwrap_or(true),
            deliver_mid_turn: file.deliver_mid_turn.unwrap_or(true),
            keep_workspaces: file.keep_workspaces.unwrap_or(false),
            sandbox: file.sandbox.unwrap_or(false),
            sandbox_allowed_domains: file.sandbox_allowed_domains.unwrap_or_default(),
            sandbox_allow_write: file
                .sandbox_allow_write
                .unwrap_or_default()
                .iter()
                .map(|p| expand_home(p).to_string_lossy().into_owned())
                .collect(),
            claude_bin: std::env::var("COLONY_CLAUDE_BIN")
                .ok()
                .or(file.claude_bin)
                .unwrap_or_else(|| "claude".into()),
            state_dir: expand_home(&state_dir),
            port: o.port.unwrap_or(0),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.goal.trim().is_empty() {
            bail!("the goal is empty (pass --goal or --goal-file)");
        }
        if self.workers == 0 || self.max_active == 0 || self.gate_slots == 0 {
            bail!("workers, max_active and gate_slots must be at least 1");
        }
        if self.hours.is_nan() || self.hours <= 0.0 {
            bail!("hours must be greater than 0");
        }
        Ok(())
    }
}

pub fn load_file(path: &Path) -> Result<FileConfig> {
    if !path.exists() {
        return Ok(FileConfig::default());
    }
    let text = std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid format in {}", path.display()))
}

/// The run directory root: the given path, or `~/.colony/runs`.
pub fn state_dir(given: Option<&str>) -> PathBuf {
    expand_home(given.unwrap_or("~/.colony/runs"))
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(p),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_overrides_file_overrides_defaults() {
        let file: FileConfig = toml::from_str("workers = 32\nverify = \"pnpm test\"\nhours = 1.5").unwrap();
        let o = Overrides {
            workers: Some(4),
            ..Default::default()
        };
        let c = Config::resolve("g".into(), "/r".into(), file, o).unwrap();
        assert_eq!(c.workers, 4);
        assert_eq!(c.max_active, 4, "defaults to min(workers, 8)");
        assert_eq!(c.verify.as_deref(), Some("pnpm test"));
        assert_eq!(c.hours, 1.5);
        assert_eq!(c.permission_mode, "acceptEdits");
    }

    #[test]
    fn max_active_defaults_to_eight_for_large_teams() {
        let o = Overrides {
            workers: Some(32),
            ..Default::default()
        };
        let c = Config::resolve("g".into(), "/r".into(), FileConfig::default(), o).unwrap();
        assert_eq!(c.max_active, 8);
    }

    #[test]
    fn defaults_scope_tools_and_enable_run_fixes() {
        let c = Config::resolve(
            "g".into(),
            "/r".into(),
            FileConfig::default(),
            Overrides::default(),
        )
        .unwrap();
        assert_eq!(c.tools, vec!["Bash", "Read", "Edit", "Write", "Glob", "Grep"]);
        for bare in ["Write", "Edit"] {
            assert!(
                !c.allowed_tools.iter().any(|t| t == bare),
                "bare {bare} would allow writes anywhere"
            );
        }
        assert!(c.allowed_tools.iter().any(|t| t == "Bash(cd *)"));
        assert!(c.fresh_session && c.deliver_mid_turn && !c.keep_workspaces && !c.sandbox);
        assert_eq!(c.idle_secs, 600);
    }

    #[test]
    fn reads_new_keys() {
        let file: FileConfig = toml::from_str(
            "tools = [\"Bash\", \"Read\"]\nfresh_session = false\ndeliver_mid_turn = false\nkeep_workspaces = true\n\
             sandbox = true\nsandbox_allowed_domains = [\"registry.npmjs.org\"]\nsandbox_allow_write = [\"~/Library/pnpm\"]",
        )
        .unwrap();
        let c = Config::resolve("g".into(), "/r".into(), file, Overrides::default()).unwrap();
        assert_eq!(c.tools, vec!["Bash", "Read"]);
        assert!(!c.fresh_session && !c.deliver_mid_turn && c.keep_workspaces && c.sandbox);
        assert_eq!(c.sandbox_allowed_domains, vec!["registry.npmjs.org"]);
        assert!(
            c.sandbox_allow_write[0].ends_with("/Library/pnpm") && !c.sandbox_allow_write[0].starts_with('~'),
            "{:?}",
            c.sandbox_allow_write
        );
    }

    #[test]
    fn rejects_unknown_keys_and_empty_goal() {
        assert!(toml::from_str::<FileConfig>("wokers = 3").is_err());
        assert!(Config::resolve(
            " ".into(),
            "/r".into(),
            FileConfig::default(),
            Overrides::default()
        )
        .is_err());
    }
}
