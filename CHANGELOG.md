# Changelog

Each version's section becomes its GitHub release notes (`.github/scripts/release-notes.sh`).

## [0.1.1] - 2026-10-30

Fixes for what the first real-scale run showed: 6 workers, a goal completed in 25.7 minutes, at $62.43
([#1](https://github.com/yusa-imit/colony/issues/1)). Each item has its evidence, root cause and verification in
[`docs/issue/`](https://github.com/yusa-imit/colony/tree/v0.1.1/docs/issue). After the fixes, a real run on the
toy crate cost $0.02, against $0.04–0.07 before.

### Cost and pacing

- One task per turn: `task_complete` now tells the worker to end its turn. Before, a single turn ran 2–5 tasks
  over 12–20 minutes.
- A worker restarts in a fresh session after each task it completes (`fresh_session`, on by default). The
  board, notes, task logs and git carry what the next task needs.
- The goal moved into the shared system prompt; `board_view` shows only its title. Repeating it in every
  `board_view` cost about 20% of the run.
- Cost is estimated live from every API call and calibrated against each turn's result, so `--max-cost-usd`
  holds mid-turn. At the cap, running turns are interrupted.
- The five-hour usage window is projected to its reset, and concurrency drops before it runs out. The
  dashboard shows the projection and the resolved model.
- Idle workers wake only when claimable work appears or someone addresses them directly. `idle_secs` now
  defaults to 600.

### Coordination

- Fixed: posting to chat marked the poster's unread messages as read.
- Direct mentions (`@w07`) and new notes reach a worker mid-task, after its next tool call, through a
  PostToolUse hook (`deliver_mid_turn`, on by default). `@all` is an announcement read at the next turn and
  wakes nobody.
- `task_claim` on a task whose dependencies are not done reserves it. The task becomes the reserver's claim
  when its dependencies finish.
- New `verify` tool runs the configured verify command on colony-main. Results are cached per commit, and
  `goal_complete` reuses them.
- When the last task is done, the worker that finished it is asked to check the goal and call
  `goal_complete`.
- The planning task asks for a short critical path and explicit dependencies on shared helpers.

### Isolation

- Write and Edit are allowed only inside the worker's own clone. Scratch files go in `.colony-tmp/`, which git
  ignores. A `colony.toml` that sets `allowed_tools` keeps its own rules.
- Workers get only the built-in tools in `tools` (default `Bash, Read, Edit, Write, Glob, Grep`). The colony
  tools load without a ToolSearch call.
- `Bash(cd *)`, `Bash(sed *)` and `Bash(./node_modules/.bin/*)` are allowed by default.
- The launching Claude Code session's own variables are removed from workers' environment, among them its
  plugin dirs, messaging socket and effort.
- Optional Bash sandbox: set `sandbox = true`, with `sandbox_allowed_domains` and `sandbox_allow_write`.

### Housekeeping

- `ws/` is removed after a completed run (`keep_workspaces` keeps it). `colony clean` removes it from finished
  runs.
- The start banner shows the model setting.
- Prebuilt archives for macOS (Apple Silicon, Intel) and Linux (x86-64, arm64; static), built and published by
  CI.

## [0.1.0] - 2026-10-28

First release: an orchestrator-free multi-agent harness. Several `claude` workers claim tasks on a shared
board, work in their own clones, and merge by pushing to a fast-forward-only hub. It implements the ideas of
Agensh ([arXiv:2609.26781](https://arxiv.org/abs/2609.26781)) for a single machine and any git repository.
