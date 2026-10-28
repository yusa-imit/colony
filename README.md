# colony

A multi-agent harness in which several `claude` processes **divide the work among themselves** on a shared
board, with no central orchestrator. It ships as a single binary (`colony`), and each worker uses the Claude
Code installation you are already signed in to.

colony brings the self-organizing structure of Agensh
([arXiv:2609.26781](https://arxiv.org/abs/2609.26781), [code](https://github.com/microsoft/Agensh)) to a
local machine; see [Citation](#citation). When one agent directs sub-agents, the orchestrator becomes a
bottleneck and a single point of failure. colony has no orchestrator. Workers read the board, claim tasks
atomically, make changes in their own clones, merge by pushing to a shared hub, and record what they learn as
notes.

```
                       colony (single binary)
   ┌───────────────────────────────────────────────────────────┐
   │  board: tasks · chat · notes · goal                       │
   │  supervisor: turn slots · usage limits · wake-ups         │
   │  MCP /mcp  ·  gate /gate  ·  dashboard /                  │
   └───────▲─────────────▲─────────────▲───────────────────────┘
           │ MCP (JSON-RPC over HTTP)  │ stream-json (stdin/stdout)
   ┌───────┴──┐   ┌──────┴───┐   ┌─────┴────┐
   │ claude w1│   │ claude w2│ … │ claude wN│   each works in its own clone of the hub
   └────┬─────┘   └────┬─────┘   └────┬─────┘
        └── git push ──┴── hub.git (colony-main, fast-forward only) ──┘
                                   │ at the end
                                   ▼
                fetched into the target repository as branch colony/<run id>
```

## Requirements

- The `claude` CLI, signed in. Whatever authentication you normally use (subscription or API key) applies.
- `git`
- The target repository's committed state is the starting point; uncommitted changes are not passed to workers.

## Install

```sh
cargo build --release          # target/release/colony — put this one file on your PATH
cp target/release/colony ~/.local/bin/
```

## Usage

```sh
cd ~/work/my-app
colony run --goal "Add a dark mode toggle to the settings page, with e2e tests" -n 6 --hours 2 --max-cost-usd 30
```

For longer requirements, use a file: `colony run --goal-file docs/task.md`.

On start, colony prints a dashboard URL (`http://127.0.0.1:<port>`). It shows worker status, the task board,
chat, shared notes, usage and cost, refreshed every two seconds.

When the run ends, the target repository has a new branch `colony/<run id>`. Your current branch and working
tree are left untouched.

```sh
git log --oneline main..colony/20261028-110652
git diff main...colony/20261028-110652
```

- Ctrl+C once: let turns in progress finish, then stop (waiting at most `grace_secs`). Twice: stop immediately.
- Whatever the stop reason, everything merged into the hub by then is kept on the branch.

### Main flags

| Flag | Default | Meaning |
|---|---|---|
| `-n, --workers` | 8 | Number of workers |
| `--max-active` | min(workers, 8) | Maximum number of workers in a turn at once |
| `--hours` | 2 | Deadline |
| `--max-cost-usd` | none | Total cost cap (the run stops when exceeded) |
| `--ramp-secs` | 15 | Delay between worker starts |
| `--model`, `--effort` | claude's default | Passed through to claude |
| `--setup` | none | Run once per worker workspace (e.g. `pnpm install --frozen-lockfile`) |
| `--verify` | none | Command run on colony-main before `goal_complete` is accepted |
| `--permission-mode` | acceptEdits | claude permission mode |
| `--base` | HEAD | Starting commit |

## `colony.toml`

Repository-specific policy (install and verify commands, allowed tools) lives in `colony.toml` at the root of
the target repository. CLI flags override it.

```toml
workers = 6
max_active = 6
hours = 2.0
model = "sonnet"
setup = "pnpm install --frozen-lockfile"
# setup does not run in the verification workspace; include installation in the command if needed.
verify = "pnpm install --frozen-lockfile && pnpm -w typecheck && pnpm -w test"
gate_slots = 3                  # heavy commands allowed to run at once
max_cost_usd = 40.0
worker_budget_usd = 8.0         # per-worker cap (claude --max-budget-usd)
idle_secs = 120
grace_secs = 300
# permission_mode = "bypassPermissions"   # sandboxed machines only
# allowed_tools = ["Read", "Edit", "Write", "Glob", "Grep", "mcp__colony", "Bash(git *)", "Bash(pnpm *)"]
# disallowed_tools = ["Agent", "AskUserQuestion"]
# setting_sources = "project,local"
```

## Worker tools

`board_view` `task_get` `task_propose` `task_claim` `task_progress` `task_release` `task_complete` `task_reopen`
`chat_post` `chat_read` `note_add` `note_search` `goal_complete`

The harness enforces a few rules:

- The board starts with one planning task, T1. The first worker to claim it decomposes the goal; the others
  sleep until tasks appear.
- Claims are atomic, and a worker holds one task at a time.
- If `task_complete` is given a commit, that commit must already be on colony-main. Pushing is merging.
- The hub rejects non-fast-forward and force pushes. Whoever arrives second rebases.
- `goal_complete` is accepted only when no task is open or claimed and `verify` passes.
- Any worker can `task_reopen` a done task that turns out to be broken. That is peer verification.

Heavy commands are wrapped as `colony gate -- <command>`, and only `gate_slots` of them run at once. This keeps
many workers on one machine from building simultaneously and grinding it to a halt. Outside the harness, the
command simply runs.

## Usage limits and concurrency

However many workers there are, they share one account's usage. colony watches the `rate_limit_event` that
claude emits (five-hour and seven-day utilization, reset time) along with 429 and overload responses, and
adjusts the pace:

- On hitting a limit, everything pauses (until the reset time when known, otherwise from one minute, doubling)
  and concurrency is halved. The blocked turn goes back to the front of the queue.
- In the warning state (`allowed_warning`), concurrency drops by one.
- As successful turns accumulate, concurrency climbs back by one at a time, up to `max_active`.

Thirty-two workers take roughly 5–10 GB of process memory. The real bottlenecks are usage limits and build load.
Start `max_active` around 8 and raise it while watching utilization on the dashboard.

## Isolation and safety

- Workers start with `--setting-sources project,local --strict-mcp-config`. Your global plugins, hooks and MCP
  servers are not loaded; the target repository's `.claude/` settings and `CLAUDE.md` are.
- The default permissions are `acceptEdits` plus an allow list (`git`, package managers, read-only tools and
  so on). Commands outside the list are denied without prompting (`--permission-prompts none`). Sub-agents
  (`Agent`) and tools that ask a human are disallowed.
- Result commits never carry AI trailers (`Co-Authored-By: Claude …`, `🤖 Generated with …`). Worker settings
  keep them from being written, a `commit-msg` hook in each clone strips them, and a `pre-receive` hook in the
  hub rejects commits that skipped the hook. Existing history in the target repository is not inspected.
- Workers operate only inside their own clones under `~/.colony/runs/<repo>-<run id>/ws/`. The target
  repository is read at start and receives one branch at the end; nothing else touches it.

## Run directory

```
~/.colony/runs/<repo>-<run id>/
├── hub.git/              shared hub (bare)
├── ws/w01 … wNN/         worker clones
├── ws/_verify/           clone used to verify goal_complete
├── agents/wNN.jsonl      raw claude stream-json (including messages sent)
├── agents/wNN.stderr.log
├── agents/wNN.setup.log
├── tools.jsonl           tool-call audit log
├── board.json            final board
└── summary.json          stop reason, cost, duration, branch
```

## Development

```sh
cargo test            # unit + hub (git) + e2e (the whole harness with three fake claude workers)
cargo clippy --all-targets
```

`examples/fake_claude.rs` is a stand-in that speaks the same stream-json as the real claude and calls the real
MCP endpoint. It exercises the whole harness (contended pushes, limit handling, wake-ups, export) in seconds,
without an LLM.

Architecture and decisions: [`docs/design.md`](docs/design.md).

## Citation

colony is an independent implementation of the ideas introduced in Agensh. It is not affiliated with
Microsoft or the paper's authors, and it contains no code from the Agensh repository. If colony is useful in
your work, please cite the original paper:

> Zhihao Zhan, Ting Song, Li Dong, Shaohan Huang, Jianxun Lian, Yan Xia, and Furu Wei.
> *Agensh: Scaling Organizational Intelligence to 1,024 Agents.* arXiv:2609.26781, 2026.
> <https://arxiv.org/abs/2609.26781>

```bibtex
@misc{zhan2026agenshscalingorganizationalintelligence,
      title={Agensh: Scaling Organizational Intelligence to 1,024 Agents},
      author={Zhihao Zhan and Ting Song and Li Dong and Shaohan Huang and Jianxun Lian and Yan Xia and Furu Wei},
      year={2026},
      eprint={2609.26781},
      archivePrefix={arXiv},
      primaryClass={cs.CL},
      url={https://arxiv.org/abs/2609.26781},
}
```

## License

[MIT](LICENSE)
