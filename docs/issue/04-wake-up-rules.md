# 04. Wake-up rules wake workers with nothing to do

Kind: waste · Area: `mcp`, `supervisor`, `prompts`

## Problem

- `task_complete` always signaled `NewWork`, which wakes every idle worker. design.md D7 says only "a completion
  that resolves a dependency" should.
- `@all` posts woke every idle worker, and `after_turn` counted an unread `@all` as a mention. So a worker that
  ended its turn without `chat_read` got another turn immediately. 9 of 20 chat messages were `@all`
  announcements.
- The IdleCheck nudge said "Help verify: pull the branch, run the checks". Every idle worker answered it by
  running the full checks (#08).
- The first idle check came after 120 s. Agensh uses 10 minutes.
- At context sizes near 300k, each such turn cost $0.13–0.30 (#05).

## Evidence

41 of 50 turns were taken by a worker holding no task: 122 API calls, $8.32 (13%).

| Turn kind | Turns | Cost |
|---|---|---|
| IdleCheck | 8 | $1.60 |
| Woken, nothing new for the worker | 6 | $2.06 |
| Continued only because an `@all` was unread | 8 | $1.18 |
| Woken by a mention | 6 | $0.89 |

## Fix

- `task_complete`, `task_propose`, `task_release` and `task_reopen` signal `NewWork` only when the set of
  claimable tasks gained a member.
- A completion that leaves no open or claimed task signals `AllDone`. The completing worker's next turn says
  "every task is done; verify and call goal_complete". Exactly one worker is asked. Without this, the last
  task's owner would end its turn (#03) and nobody would close the goal until an idle check.
- Chat: only direct `@wNN` mentions wake a worker or keep it going. `@all` is an announcement, read at the next
  turn start (Agensh: channel messages at loop start, direct messages mid-turn). The harness's own `@all`
  (refused `goal_complete`) still wakes everyone.
- `after_turn` counts only undelivered direct mentions (see #07).
- IdleCheck text: propose missing work if you see any, otherwise end the turn; use `verify` (#08) instead of
  re-running checks.
- `idle_secs` defaults to 600 (Agensh: 10 minutes).
- The decision logic is a pure function, `supervisor::decide_next`, with unit tests.

## Verification

- `supervisor::tests::decide_*`, `board::tests::claimable_ids_*`.
- e2e: the run still finishes with `idle_secs = 1`.
