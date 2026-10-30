# colony design

## 1. What and why

Orchestration in which one agent directs sub-agents bottlenecks on the orchestrator as it scales. The
orchestrator's context window has to hold the state of every subtask, and when it decomposes badly or
stalls, the whole team stalls with it.

Agensh (Microsoft, arXiv 2609.26781) removes the orchestrator: workers organize themselves through a
shared workspace, a messaging channel and shared context, and the system scales to 1,024 agents. The
published implementation is tied to Linux servers, Docker and ProgramBench tasks. colony brings the same
structure to **a single Mac, the `claude` you already use, and any git repository**.

### Goals

- No orchestrator. The harness manages processes; workers decide who does what by reading the board.
- One binary. Hand a colleague a single file. The only runtime dependencies are `claude` and `git`.
- Run for hours within usage limits. Hitting a limit slows the team down instead of failing it.
- Leave the user's repository alone. The result arrives as a single branch.

### Non-goals

- Distributed execution across machines (§7 sketches the direction).
- Benchmark reproduction. No evaluation layer such as ProgramBench.
- Sandboxing the worker runtime. Permissions are delegated to Claude Code's permission modes and allow lists.

## 2. Mapping from Agensh

| Agensh | colony |
|---|---|
| Shared workspace (Gitea) | A bare repository `hub.git` per run, integration branch `colony-main` |
| Messaging interface (Mattermost) | Board chat (`chat_post`/`chat_read`). `@wNN` is a direct message, delivered mid-turn (D12); `@all` is a channel announcement, read at the next turn |
| Shared context (adapted DeLM) | Board notes (`note_add`/`note_search`), delivered mid-turn (D12), and task logs (the plan given at claim time, or a reservation, is the work intent) |
| Worker loop (Copilot CLI over ACP) | A `claude -p` stream-json process, nudged between turns, one task per turn, a fresh session per task (D5) |
| Collaboration loop: gather context → claim → act → verify → merge | The protocol in the system prompt (`prompts.rs`) plus rules the harness enforces (§4) |
| Worker start-up ramp (30 s apart for the first 120) | `ramp_secs` |

## 3. Architecture

```
colony (single process, tokio)
├── server      127.0.0.1:<port>
│   ├── POST /mcp        worker tools; JSON-RPC 2.0, stateless
│   ├── GET  /gate/hold  heavy-command slot, held while the connection is open
│   ├── GET  /inbox      what a worker has not been shown yet; read by its PostToolUse hook (D12)
│   ├── GET  /api/state  dashboard data
│   └── GET  /           dashboard (embedded in the binary)
├── board       tasks, chat, notes, goal; one in-memory value behind one mutex, so every change is serialized
├── supervisor  worker lifecycle, turn slots, limit handling, wake-ups, shutdown
└── hub         git: hub creation, worker clones, merge checks, result export

claude ×N       stream-json over stdin/stdout; attached to /mcp through its MCP config (header x-colony-agent)
```

`board`, `stream` and `prompts` perform no IO. They take time and git verdicts as values, which keeps every
collaboration rule pinned down by unit tests.

## 4. Decisions

### D1. Language: Rust, single binary

The deliverable must be one file. The toolchain is pinned (1.98.1) and releases use a size-optimized profile
(strip, LTO). The dashboard HTML is compiled in with `include_str!`. A version tag builds and publishes the
release (`.github/workflows/release.yml`): macOS for Apple Silicon and Intel, and Linux for x86-64 and arm64 as
static musl binaries. The dependency tree is pure Rust on these targets, so no C toolchain is involved.

### D2. Worker ↔ harness protocol: MCP (JSON-RPC over HTTP), not gRPC

Claude Code's MCP transports are `stdio`, `sse` and `http`, all JSON-RPC 2.0. Using gRPC would require an
extra stdio↔gRPC translation process per worker (32 workers, 32 proxies). The traffic is local and amounts to
a few calls per worker per minute, so gRPC's performance advantages have nowhere to show, while tonic, prost
and a `.proto` build step would make both the binary and the build heavier. With JSON, `curl` can hit the
endpoint directly, and the audit log and dashboard share one format.

gRPC is worth revisiting when the node-to-node link of §7 (harness ↔ harness) exists. Even then, the worker
side stays MCP.

### D3. MCP implemented directly (no SDK), stateless Streamable HTTP

The only methods needed are `initialize`, `ping`, `tools/list` and `tools/call`, plus notifications (202).
One request per POST; responses are `application/json`. Sessions and SSE are unused, so GET and DELETE return
405. Workers are identified by `x-colony-agent`, which claude attaches from the `headers` field of its MCP
config. The server echoes the protocol version the client sends, since it relies on no version-specific
features. Verified against claude 2.1.312: the server reports `connected` and exposes all its tools (14 now).

### D4. Merging is `git push`; the hub accepts fast-forwards only

The hub enables `receive.denyNonFastForwards` and `receive.denyDeletes`. The first worker to push wins; the
other rebases and pushes again. The harness has no merge queue and no conflict resolver: workers already know
git. `task_complete` accepts a commit only if it is an ancestor of `colony-main`, so "done" means "merged".

Worker workspaces are clones, not worktrees. Worktrees share refs, which would erase the push semantics
(rejection). Local clones hard-link objects and are cheap.

### D5. Processes live for one task; the supervisor hands out turns

Workers start with `--input-format stream-json`. When a turn ends (`result`), the supervisor writes the next
message to stdin, so context and the prompt cache carry over within a task. One task is one turn: the
`task_complete` answer tells the worker to end its turn (#03 in `docs/issue`).

When a worker ends a turn holding no claim after completing a task in its session, the supervisor closes its
stdin and starts it again under a new session id (`fresh_session`, default on). Across tasks, a persistent
session cost more than it saved: every call of a later task re-read the earlier tasks' transcripts (a real run:
context p50 185k, max 411k, about a quarter of the spend). The board, notes, task logs and git carry what the
next task needs; the goal is in the shared system prompt, so a fresh session starts from a cached prefix.
Agensh's workers ran with a 272k input cap, which bounded the same effect.

Because the harness assigns session ids (`--session-id`), a crashed process resumes the same conversation
with `--resume` (up to three times).

Concurrency counts **workers in a turn**, not processes. All N processes are alive; only `limit`
(≤ `max_active`) turn slots run at once, and the rest wait in a queue.

### D6. Adaptive concurrency driven by usage limits

claude emits a `rate_limit_event` every turn (status, reset time, five-hour and seven-day utilization).

- `rejected`, or a turn ending with 429, 529 or limit wording: pause everything (until the reset time when
  known, otherwise from one minute, doubling up to 15 minutes) and halve `limit`. The blocked turn goes back to
  the front of the queue. One incident is never counted twice.
- `allowed_warning`: decrease `limit` by one (at most once a minute).
- After `2 × limit` consecutive successful turns, increase `limit` by one (up to `max_active`).

This is AIMD: multiplicative decrease, additive increase. It was motivated by an earlier attempt to launch
35 sub-agents at once, 24 of which failed on session limits. The same experience sets the `max_active`
default to `min(workers, 8)`.

### D7. Wake-ups: idle workers sleep until there is something for them

A worker whose turn ends gets another turn immediately if it holds a claim, has an undelivered direct mention,
or has claimable work (`supervisor::decide_next`). Otherwise it sleeps (`idle`). Idle workers are woken only
when the set of claimable tasks gains a member (a proposal, release, reopen, or a completion that resolves a
dependency); a direct mention wakes the worker mentioned; an `@all` announcement wakes nobody. When a
completion leaves no open or claimed task, the completing worker's next turn asks it to check the goal and
call `goal_complete`. If nothing happens, the worker is checked on after `idle_secs` (default 600, as in
Agensh), doubling up to 15 minutes, and asked to propose missing work if it sees any. After two consecutive
turns that end without a claim, the worker sleeps even if claimable work exists, which stops loops that burn
turns without doing anything.

Waking every worker on every board change was measured in a real run: 41 of 50 turns held no task.

Nudges are built from the board at the moment they leave the queue, so they never go stale while waiting.

### D8. Collaboration rules enforced by the board

- The board starts with a single planning task, T1. Because claims are atomic, exactly one worker decomposes
  the goal; the rest have nothing to do, sleep, and wake when tasks are proposed. This adds no orchestrator; it
  makes decomposition itself a claimable task. (Observed in a real run: with a 5-second ramp, two workers saw
  the empty board at the same time and each produced the same plan, one in Japanese and one in Korean. Title
  similarity cannot catch duplicates across languages.)
- The planning instructions ask for a short critical path: a small shared skeleton, wrap-up work split so its
  independent parts start early, and shared helpers in one early task the others depend on.
- Claims are atomic, one per worker.
- A task whose dependencies are not done cannot be claimed. `task_claim` on it reserves it (one reservation
  per worker); when its last dependency completes it becomes the reserver's claim if the reserver holds
  nothing, and otherwise the reservation lapses. (Observed in a real run: two workers drafted the same
  wrap-up task because intent could only be stated in chat.) A dependency is an existing id, an earlier item of
  the same proposal (`#1`), or the title of either. References only point backwards, so cycles are impossible.
- A proposal whose title resembles open work (token Jaccard ≥ 0.8) is refused; `force` overrides. Items in the
  same batch are not compared with each other (at 0.6, a sound plan was refused because its titles shared
  boilerplate).
- `task_propose` also accepts input outside its schema: a single task sent flat as `{title, description}`,
  `tasks` encoded as a JSON string, and comma-separated `deps`. Models sent all of these in real runs, and every
  refusal costs a turn.
- Common argument aliases are accepted for the same reason (`message` for `text`, `task_id` for `id`, `note`
  for `text`), and the `goal_complete` summary is optional.
- Only the owner can complete a task, and a commit passed with it must already be merged. The answer tells
  the worker to end its turn, and asks for a note when the worker added none since claiming (Agensh reminds
  workers whose turn published nothing to shared context).
- Anyone can reopen a done task (peer verification); the previous owner is notified in chat.
- The goal can be completed only when no task is open or claimed, at least one task is done, and `verify`
  passes. A failed `verify` is announced to `@all`.
- The harness releases any claim held by a worker that has failed.

The rules are not left to the protocol text alone: violations come back as refusals, and every refusal says
what to do next.

### D9. Heavy-command gate

`colony gate -- <command>` opens a connection to `/gate/hold` and runs the command once it receives `ok`. The
server reclaims the slot when the connection closes, and writes a keepalive every 5 seconds so it notices dead
sockets quickly. A slot therefore cannot leak even if the worker's Bash call times out and is killed. Outside the
harness the command simply runs. The colony binary's directory is prepended to each worker's PATH.

### D10. Isolation

Agensh's workers run in containers and may use whatever is at hand. colony runs on the host, so Claude Code's
permission system is the boundary, and a real run found it wrong in both directions (#10 in `docs/issue`).

- `--setting-sources project,local --strict-mcp-config`: the user's global plugins, hooks and MCP servers are
  off; only the target repository's settings apply. This also keeps 32 workers from each starting the user's
  plugins. The launching Claude Code session's own variables (`CLAUDE_CODE_PLUGIN_DIRS`, its messaging
  socket, `CLAUDE_EFFORT` …) are removed from the workers' environment; inherited, they loaded a terminal
  plugin into every worker despite `--setting-sources`.
- `--tools` limits the built-in tools to `Bash, Read, Edit, Write, Glob, Grep`. The full set (32 tools) acted
  outside the hub and added to every prompt, and with `ToolSearch` absent the colony tools load up front.
- `--permission-mode acceptEdits --permission-prompts none` plus an allow list; anything outside it is denied.
  There is no bare `Write`/`Edit` rule: each worker gets `Edit(//<clone>/**)` and `Write(//<clone>/**)`, so
  file tools cannot write outside the clone (verified on claude 2.1.312; the bare rules let a real run write
  to `/tmp` 30 times). `Bash(cd *)` and `Bash(sed *)` are allowed (denied 19 and 2 times). Scratch files go in
  `.colony-tmp/`, excluded through the clone's `.git/info/exclude`. `Agent` (sub-agents) and
  `AskUserQuestion` are disallowed.
- `sandbox = true` switches Bash to Claude Code's sandbox: commands are auto-allowed, writes are limited to the
  clone, the hub, temp dirs and `sandbox_allow_write`, and the network to localhost (the gate) and
  `sandbox_allowed_domains`. It is the closest local equivalent of Agensh's containers, but off by default
  because package stores and registries outside the clone differ per machine.
- Allow-list rules match on the command's first word, so an environment prefix such as `GIT_EDITOR=true git …`
  is denied. The harness sets the unattended-git variables (`GIT_EDITOR=true` and others) itself and the protocol
  forbids prefixes. (Observed in a real run: a worker was denied while resolving a rebase conflict with that
  prefix.)
- `--exclude-dynamic-system-prompt-sections`: the per-worker cwd moves into the first message so every worker
  has an identical system prompt and they share the prompt cache.

### D11. No AI trailers (principle)

The result branch lands in a person's repository. AI trailers in commit messages are blocked in three layers:

1. Worker `--settings {"includeCoAuthoredBy":false,"attribution":{"commit":"","pr":""}}`: the trailer is never
   written. (Observed: with the same instruction, no settings produce `Co-Authored-By: Claude Haiku 5.5`; these
   settings produce none.)
2. A `commit-msg` hook in each worker clone strips any trailer line that still appears, along with trailing
   blank lines. It does not run if an install step (husky and the like) changes `core.hooksPath`.
3. A `pre-receive` hook in the hub (with `core.hooksPath` pinned to an absolute path) rejects commits that
   skipped the hook. It checks new commits only, so trailers in the target repository's existing history never
   block a push.

The pattern lives in one place, `hub::AI_TRAILER_PATTERN` (Claude, Anthropic, Copilot, OpenAI, Codex, Gemini).

### D12. Mid-turn delivery through a PostToolUse hook

Agensh appends direct messages and new shared-context entries to the result of a worker's next infrastructure
tool call (§B.2). colony gives each worker a `PostToolUse` hook (in `--settings`, matcher `*`) that runs
`colony inbox`, which asks `GET /inbox` for what the worker has not been shown (direct mentions, other workers'
notes) and returns it as `additionalContext`. The board marks each item delivered, so it appears once,
whether mid-turn or in the next nudge. Verified on claude 2.1.312, also under `--setting-sources
project,local`.

Writing a user message to a working process's stdin is also absorbed at the next tool boundary (verified), but
a message that races the end of a turn starts a turn the supervisor did not ask for. The hook runs only inside
a turn.

### D13. Live cost

`result.total_cost_usd` arrives only at turn end. Each assistant message carries its input-side usage as it
starts, so the supervisor accrues it per message and prices it with `cost::Meter`. The meter is calibrated
against every completed turn's real cost and exact usage, with weights fitted on a real run: input 1, 5m write
1.25, 1h write 2, read 0.05, output 5. Before any turn completes it assumes $4/MTok, which errs high. At the
cap, running turns are interrupted with a stream-json `control_request` (`interrupt`), which ends a turn within
50 ms, and the grace period is at most 30 s.

### D14. Shared verification

The `verify` tool runs the configured verify command on colony-main in the verification clone, through the
gate, and caches the result per commit. Repeated calls on the same commit, from anyone, and `goal_complete`
reuse it. Under Agensh's time budget, redundant checks buy robustness; under a cost cap they were four or more
full runs on the same commit at the end of a real run.

## 5. Run lifecycle

1. Check `claude --version`, resolve the repository root, warn about uncommitted changes.
2. Create the hub (`clone --bare`, `colony-main` = the base commit) and, if `verify` is set, a verification clone.
3. Start the server and the supervisor.
4. Every `ramp_secs`, prepare a worker (clone, then `setup` inside the gate), start its process, send the first
   message.
5. Stop reasons: goal complete, deadline, cost cap, Ctrl+C, all workers failed. Stopping closes every stdin.
   Turns in progress finish; after a grace period (`grace_secs`, at most 60 seconds when the goal is complete)
   remaining processes are killed.
6. If the hub moved past the base, fetch it into the target repository as `colony/<run id>`. Write `board.json`
   and `summary.json`. After a completed run, remove `ws/` (`keep_workspaces` keeps it; `colony clean` removes
   it from finished runs later).

## 6. Field notes

### Runs 1–3 (claude 2.1.312, Haiku 5.5, 3 workers, toy Rust crate)

| Run | Outcome | Finding → action |
|---|---|---|
| 1 (20 s ramp) | Goal complete in 1m30s, $0.06; `cargo test` passes on the result branch | One worker was blocked mid-rebase when `GIT_EDITOR=true git …` was denied; another worker offered in chat to take over, reimplemented the task and merged it. Action: inject unattended-git variables (D10) |
| 2 (5 s ramp) | Goal complete, $0.07; `cargo test` passes; two conflicts resolved with `rebase --continue` | Duplicate plans, four flat `task_propose` inputs refused, a sound batch misjudged as a duplicate → planning task, lenient input and a higher threshold (D8) |
| 3 (3 s ramp) | Goal complete in 50 s, $0.04; `cargo test` passes; one plan, no permission denials | Seven argument-name mismatches (`message`, `task_id`, `note`, an empty `goal_complete`) → accept common aliases, make the `goal_complete` summary optional |

In all three runs the workers coordinated through chat without instruction: they proposed where to put new
lines in `lib.rs` so rebases would conflict less, took over a blocked peer's task, and independently ran the
tests on `colony-main` before declaring the goal complete.

### First real-scale run (claude 2.1.312, Opus 5.5, 6 workers, TypeScript monorepo)

Goal complete in 25.7 min with a correct result, but $62.43. The toy runs could not show any of the problems:
their goal was 294 characters and every turn lasted under a minute. Analysis, measurements and fixes are in
[`docs/issue/`](issue/README.md) (run report: issue #1). In short:

- Turns ran 12–20 min and spanned 2–5 tasks. The `task_complete` answer said "pick your next task", against
  the protocol. Cost accounting, nudges and wake-ups, all tied to turn ends, became 20 minutes coarse.
- Each `board_view` repeated the 25k-character goal (≈14.5k tokens, ≈20% of the run).
- Posting to chat marked unread messages as read (29 deliveries lost).
- Waking every idle worker on every change: 41 of 50 turns held no task.
- The allow list both denied harmless forms (41 times) and let file tools write outside the clones.

### Run 4, after the fixes (claude 2.1.312, Haiku 5.5, 3 workers, toy Rust crate, same goal as runs 1–3)

Goal complete in 43 s, $0.02 (runs 1–3: $0.04–0.07), 9 turns. `cargo test` passes on the result branch and no
commit carries a trailer.

| Check | Observed |
|---|---|
| One task per turn, fresh session per task | every worker completed one task per turn; w1 ran 3 sessions, w2 and w3 two each |
| Tool surface | `Bash, Edit, Glob, Grep, Read, Write` + colony tools; no `ToolSearch` call; the terminal's inline plugin no longer loaded |
| Kickoff context | 12–15k tokens (runs 1–3: 19–28k) |
| Permission denials | 0 |
| Mid-turn delivery | w3's note N1 reached w1 after an `Edit` and w2 after a `task_claim` (`hook_additional_context`), once each |
| Shared verification | `verify` ran once on the final commit; `goal_complete` reused it |
| Note reminder | every `task_complete` answer asked for a note; w3 then added one |
| Planning | the planner left the README and final-check tasks without deps and said so in chat; the planning guidance alone did not prevent it |
| Cleanup | `ws/` removed after the run |

## 7. Future directions

- **Multiple machines**: a node harness on each machine starts workers, and the board lives in one place (a
  coordinator node). Workers keep using MCP. The node ↔ coordinator link is where gRPC (bidirectional streams
  for nudges and events) should be evaluated (D2). The hub is a git remote and distributes as is.
- **Resuming a run**: replay `tools.jsonl` to rebuild the board and `--resume` workers by session id.
- **Note search**: token overlap today; something like BM25 once notes number in the hundreds.
- **Observing role specialization**: aggregate emergent roles reported by Agensh (such as integrators) from
  `tools.jsonl` and surface them on the dashboard.
