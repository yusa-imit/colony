//! `colony run` composition root: creates the hub, starts the server and supervisor, and hands over
//! the result branch when the run ends.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tokio::sync::mpsc;

use crate::board::Board;
use crate::config::Config;
use crate::gate::Gate;
use crate::hub::{self, Hub};
use crate::prompts::{self, ProtocolVars};
use crate::server::{self, Shared, Verify};
use crate::supervisor::{log, Outcome, RunCtx, StopReason, SupEvent, Supervisor};

pub struct Finished {
    pub outcome: Outcome,
    pub branch: Option<String>,
    pub run_dir: std::path::PathBuf,
}

pub async fn run(cfg: Config) -> Result<Finished> {
    check_claude(&cfg.claude_bin).await?;
    let repo = hub::repo_root(&cfg.repo).await?;
    let repo_name = repo
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "repo".into());
    let run_id = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let run_dir = cfg.state_dir.join(format!("{repo_name}-{run_id}"));
    tokio::fs::create_dir_all(run_dir.join("agents"))
        .await
        .context("failed to create the run directory")?;
    tokio::fs::create_dir_all(run_dir.join("ws")).await?;

    if hub::is_dirty(&repo).await? {
        log("warning: uncommitted changes are not passed to workers (the run starts from the committed state)");
    }
    let (hub, base_sha) = Hub::create(&repo, &cfg.base, &run_dir).await?;
    let email = hub::user_email(&repo).await;
    let verify = match &cfg.verify {
        Some(command) => {
            let workspace = run_dir.join("ws").join("_verify");
            hub.clone_workspace(&workspace, "verify", &email).await?;
            Some(Verify {
                command: command.clone(),
                workspace,
            })
        }
        None => None,
    };

    let (tx, rx) = mpsc::unbounded_channel();
    let tool_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("tools.jsonl"))
        .ok();
    let app = Arc::new(Shared::new(
        Board::seeded(&cfg.goal),
        hub,
        Gate::new(cfg.gate_slots),
        verify,
        tx.clone(),
        tool_log,
    ));
    let (addr, server) = server::serve(app.clone(), cfg.port).await?;
    let server_url = format!("http://{addr}");

    let interrupt_tx = tx.clone();
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if interrupt_tx.send(SupEvent::Interrupt).is_err() {
                break;
            }
        }
    });

    eprintln!();
    eprintln!(
        "  colony {}  ·  {repo_name} @ {}",
        env!("CARGO_PKG_VERSION"),
        &base_sha[..8]
    );
    eprintln!("  dashboard   {server_url}");
    eprintln!(
        "  workers     {} (at most {} active, started {}s apart)",
        cfg.workers, cfg.max_active, cfg.ramp_secs
    );
    eprintln!(
        "  deadline    {}  ·  cost cap {}",
        prompts::fmt_duration(std::time::Duration::from_secs_f64(cfg.hours * 3600.0)),
        cfg.max_cost_usd
            .map(|c| format!("${c}"))
            .unwrap_or_else(|| "none".into())
    );
    eprintln!("  run dir     {}", run_dir.display());
    eprintln!("  Ctrl+C once: stop after in-flight turns finish · twice: stop immediately");
    eprintln!();

    let system_prompt = prompts::system_prompt(&ProtocolVars {
        workers: cfg.workers,
        verify: cfg.verify.as_deref(),
    });
    let ctx = Arc::new(RunCtx {
        repo_label: repo_name.clone(),
        run_id: run_id.clone(),
        run_dir: run_dir.clone(),
        server_url,
        email,
        system_prompt,
        cfg,
    });
    let outcome = Supervisor::new(ctx, app.clone(), tx, rx).run().await;

    let head = app.hub.head().await?;
    let branch = if head == base_sha {
        None
    } else {
        let branch = format!("colony/{run_id}");
        app.hub.export(&repo, &branch).await?;
        Some(branch)
    };
    let snapshot = app.board().snapshot();
    tokio::fs::write(run_dir.join("board.json"), serde_json::to_vec_pretty(&snapshot)?).await?;
    let summary = json!({
        "outcome": outcome,
        "branch": branch,
        "base": base_sha,
        "head": head,
        "status": &*app.status.lock().map_err(|_| anyhow::anyhow!("status lock"))?,
    });
    tokio::fs::write(run_dir.join("summary.json"), serde_json::to_vec_pretty(&summary)?).await?;
    server.abort();
    Ok(Finished {
        outcome,
        branch,
        run_dir,
    })
}

pub fn print_summary(f: &Finished) {
    let o = &f.outcome;
    eprintln!();
    eprintln!("  stopped: {}", o.reason.label());
    eprintln!(
        "  time {} · turns {} · cost ${:.2}",
        prompts::fmt_duration(std::time::Duration::from_secs(o.elapsed_secs)),
        o.turns,
        o.total_cost_usd
    );
    match &f.branch {
        Some(b) => eprintln!("  result branch: {b}  (git log {b} / git diff HEAD...{b})"),
        None => eprintln!("  result branch: none — nothing was merged into colony-main"),
    }
    eprintln!("  run dir: {}", f.run_dir.display());
    if o.reason != StopReason::Complete {
        eprintln!("  See board.json for the remaining work.");
    }
}

async fn check_claude(bin: &str) -> Result<()> {
    match tokio::process::Command::new(bin).arg("--version").output().await {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => bail!(
            "`{bin} --version` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => bail!("claude CLI not found (`{bin}`): {e}. Install Claude Code or set COLONY_CLAUDE_BIN"),
    }
}
