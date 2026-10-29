# Issues found in the first real-scale run

Source: run report [#1](https://github.com/yusa-imit/colony/issues/1) (6 workers, 25.7 min, $62.43, goal complete)
and a second pass over the same run's logs (`agents/*.jsonl`, `tools.jsonl`, `board.json`), read against the Agensh paper (arXiv 2609.26781, §2 and appendices A–C).

## Why the paper's structure underperformed here

The collaboration behaviors the paper describes did appear: workers agreed on interfaces in chat, prepared
work while waiting, and resolved every rebase conflict themselves. What failed was the plumbing under them, for
reasons the toy runs in the design notes could not expose (294-character goal, three Haiku workers, about one
minute per run, every turn under a minute):

- **Turn granularity.** colony does cost accounting, message delivery, wake-ups and pacing at turn
  boundaries, assuming one turn ≈ one task. Real turns lasted 12–20 minutes and spanned 2–5 tasks (#03).
  Agensh delivers messages mid-turn instead (§B.2), so long turns cost it nothing (#07).
- **Resource model.** Agensh optimizes quality under a fixed time budget on a 272k-token context cap; cost is
  not a metric. colony runs a finite goal on one subscription with a cost cap and a 1M-token context. The same
  mechanisms (persistent sessions, waking everyone, everyone verifying) became the main costs (#02, #04, #05,
  #08).
- **Isolation boundary.** Agensh workers run in containers with "whatever is at hand". colony replaced the
  container with a first-word allow list, which both denied harmless commands and let file tools write
  anywhere (#10).
- **Bugs invisible at toy scale** (#01, #02).

## Index

| # | Title | Kind | Measured impact in the run | Status |
|---|---|---|---|---|
| [01](01-chat-cursor-drops-unread.md) | Posting to chat marks unread messages as read | bug | 29 message deliveries lost; duplicated README work | fixed |
| [02](02-board-view-repeats-goal.md) | `board_view` repeats the whole goal on every call | cost | ≈14.5k tokens per call, ≈$12.7 (20%) | fixed |
| [03](03-one-task-per-turn.md) | `task_complete` tells workers to keep going in the same turn | root cause | 80% of spend in six 12–20 min turns | fixed |
| [04](04-wake-up-rules.md) | Wake-up rules wake workers with nothing to do | waste | 41 of 50 turns held no task; $8.32 | fixed |
| [05](05-fresh-session-per-task.md) | Context carried across tasks | cost | ≈$14.7–17.8 of cache reads | fixed |
| [06](06-live-cost-and-budget-stop.md) | Cost cap reacts 10+ minutes late | safety | overrun +41% to +444% depending on cap | fixed |
| [07](07-mid-turn-delivery.md) | No mid-turn delivery of messages and notes | coordination | a peer's claim seen 2 min late; duplicate implementation | fixed |
| [08](08-shared-verification.md) | Every idle worker re-runs the full verification | waste | 4+ redundant full checks at the end | fixed |
| [09](09-reserve-blocked-tasks.md) | Intent on a blocked task cannot be recorded | coordination | two README drafts for one task | fixed |
| [10](10-permissions-and-isolation.md) | Allow list denies harmless forms; file tools unscoped; env leaks | isolation | 41 denials; 30 writes to `/tmp`; parent session's plugin and effort leaked | fixed; sandbox opt-in |
| [11](11-planning-guidance.md) | One-shot plan with serial head and tail, internal deps missed | coordination | 19 + 13 waiting turns; one function written twice | prompt changed; partly effective |
| [12](12-rate-limit-projection.md) | No projection of the five-hour window | safety | 0.29 → 0.76 of the window in 26 min | fixed |
| [13](13-housekeeping.md) | Workspaces left behind, model not shown, ToolSearch round trip | housekeeping | 2.2 GB left in `ws/` | fixed |

All fixes are covered by `cargo test`: unit tests per issue, and the e2e run exercises fresh sessions, the
shared verify cache and workspace cleanup (and reservations when timing leads a worker there). Field check: [design.md §6, run 4](../design.md#6-field-notes).

## Claude Code behaviors verified for these fixes (claude 2.1.312, Haiku probes)

| Behavior | Result | Used by |
|---|---|---|
| A user message written to stdin during a turn | absorbed at the next tool boundary of the same turn (one `result`) | #07 (considered) |
| `PostToolUse` hook from `--settings` returning `hookSpecificOutput.additionalContext` | reaches the model, also under `--setting-sources project,local` | #07 |
| `{"type":"control_request","request":{"subtype":"interrupt"}}` on stdin | ends the running turn within 50 ms (`error_during_execution`) | #06 |
| `result.queued_turn_count`, assistant `message.usage` | present; `usage` repeats per stream line of one message and carries the starting `output_tokens` only | #06 |
| `acceptEdits` with no bare `Write`/`Edit` allow rule | writes inside the cwd pass, outside are denied | #10 |
| `Write(//<abs>/**)` in `manual` mode | inside passes, outside denied | #10 |
| `Bash(cd *)` | `cd sub && ls` passes | #10 |
| `--tools Bash,Read,Edit,Write,Glob,Grep` with an HTTP MCP server | built-ins limited to the list; MCP tools load up front, no ToolSearch | #10, #13 |
| `sandbox` settings with `network.allowedDomains: ["127.0.0.1","localhost"]` | Bash auto-allowed, localhost reachable (gate), `git push` to an `allowWrite` path works; temp dirs stay writable | #10 |
| Cost weights fitted on the run (Opus 5.5) | input $4, 1h cache write $8, cache read $0.20, output $20 per MTok (exact on all 50 turns) | #06 |
