use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use colony::config::{self, Config, Overrides};
use colony::supervisor::StopReason;

#[derive(Parser)]
#[command(
    name = "colony",
    version,
    about = "A harness in which several claude workers divide the work among themselves on a shared board"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Hand one goal to a team of workers. The result lands in the target repo as branch `colony/<run id>`.
    Run(Box<RunArgs>),
    /// Run a heavy command inside a gate slot (used by workers). Example: colony gate -- pnpm test
    Gate {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Print what this worker has not been shown yet, as PostToolUse hook output (used by workers' hooks)
    #[command(hide = true)]
    Inbox,
    /// Remove the workspaces (ws/) of finished runs. Logs, board and hub are kept
    Clean {
        /// Where run directories are kept (default ~/.colony/runs)
        #[arg(long)]
        state_dir: Option<String>,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Goal text
    #[arg(long, conflicts_with = "goal_file", required_unless_present = "goal_file")]
    goal: Option<String>,
    /// File containing the goal (for long requirements)
    #[arg(long)]
    goal_file: Option<PathBuf>,
    /// Target git repository
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Config file. Defaults to <repo>/colony.toml
    #[arg(long)]
    config: Option<PathBuf>,
    /// Number of workers
    #[arg(short = 'n', long)]
    workers: Option<usize>,
    /// Maximum workers taking a turn at once (lowered automatically when usage limits are hit)
    #[arg(long)]
    max_active: Option<usize>,
    /// Deadline in hours
    #[arg(long)]
    hours: Option<f64>,
    /// Seconds between worker starts. Gives the first worker time to break down the goal
    #[arg(long)]
    ramp_secs: Option<u64>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
    /// Value for claude --permission-mode (acceptEdits · auto · bypassPermissions …)
    #[arg(long)]
    permission_mode: Option<String>,
    /// Command run once in each worker workspace (e.g. pnpm install --frozen-lockfile)
    #[arg(long)]
    setup: Option<String>,
    /// Verification command run on colony-main before goal_complete is accepted
    #[arg(long)]
    verify: Option<String>,
    /// Number of concurrent colony gate slots
    #[arg(long)]
    gate_slots: Option<usize>,
    /// Total cost cap in USD. The run stops when it is exceeded
    #[arg(long)]
    max_cost_usd: Option<f64>,
    /// Starting commit (default HEAD)
    #[arg(long)]
    base: Option<String>,
    /// First interval (seconds) before an idle worker is checked again. Doubles each time, up to 15 minutes
    #[arg(long)]
    idle_secs: Option<u64>,
    /// Where run directories are kept (default ~/.colony/runs)
    #[arg(long)]
    state_dir: Option<String>,
    /// Port for the dashboard and MCP endpoint (default: random)
    #[arg(long)]
    port: Option<u16>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Gate { cmd } => ExitCode::from(colony::gate::run_gated(&cmd).clamp(0, 255) as u8),
        Cmd::Inbox => ExitCode::from(colony::inbox::run_inbox_hook().clamp(0, 255) as u8),
        Cmd::Clean { state_dir } => {
            let dir = config::state_dir(state_dir.as_deref());
            match colony::run::clean(&dir) {
                Ok(cleaned) => {
                    for d in &cleaned {
                        eprintln!("removed {}", d.join("ws").display());
                    }
                    eprintln!(
                        "{} finished run(s) cleaned under {}",
                        cleaned.len(),
                        dir.display()
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("colony: {e:#}");
                    ExitCode::from(2)
                }
            }
        }
        Cmd::Run(args) => match run(args) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("colony: {e:#}");
                ExitCode::from(2)
            }
        },
    }
}

fn run(args: Box<RunArgs>) -> Result<ExitCode> {
    let args = *args;
    let goal = match (&args.goal, &args.goal_file) {
        (Some(g), _) => g.clone(),
        (None, Some(p)) => {
            std::fs::read_to_string(p).with_context(|| format!("failed to read {}", p.display()))?
        }
        (None, None) => unreachable!("clap requires one of --goal / --goal-file"),
    };
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| args.repo.join(config::CONFIG_FILE));
    let file = config::load_file(&config_path)?;
    let overrides = Overrides {
        workers: args.workers,
        max_active: args.max_active,
        ramp_secs: args.ramp_secs,
        hours: args.hours,
        model: args.model,
        effort: args.effort,
        permission_mode: args.permission_mode,
        setup: args.setup,
        verify: args.verify,
        gate_slots: args.gate_slots,
        max_cost_usd: args.max_cost_usd,
        base: args.base,
        idle_secs: args.idle_secs,
        state_dir: args.state_dir,
        port: args.port,
    };
    let cfg = Config::resolve(goal, args.repo, file, overrides)?;
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let finished = rt.block_on(colony::run::run(cfg))?;
    colony::run::print_summary(&finished);
    Ok(if finished.outcome.reason == StopReason::Complete {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
