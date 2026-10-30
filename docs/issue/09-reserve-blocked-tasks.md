# 09. Intent on a blocked task cannot be recorded

Kind: coordination · Area: `board`, `mcp`, `prompts`

## Problem

Agensh records a worker's intent as a CLAIM in shared context. colony's claim is atomic, but only for claimable
tasks. A worker that wants a task still waiting on dependencies could only say so in chat, which is not binding
and, with #01, not even reliably delivered.

## Evidence

- Wrap-up task T22 waited on 19 tasks.
- w1 announced it would take T22 and drafted the README locally (21.7 min).
- w2 independently drafted the same README and claimed T22 the moment it opened (23.2 min).
- w1's claim 0.1 min later was refused.

## Fix

- `task_claim` on an open task whose dependencies are not done **reserves** it:
  - one reservation per worker;
  - refused if another worker already reserves the task.
- The board shows `open, waits T17, reserved w1`; the worker's view and nudge show its reservation.
- When the last dependency completes:
  - if the reserver holds no claim, the task becomes its claim automatically, and the reserver is woken;
  - otherwise the reservation lapses (logged) and the task is claimable by anyone.
- `task_release` on a reserved task drops the reservation.
- A failed worker's reservations are dropped, like its claims.

Preparing locally for a reserved task is allowed; pushing waits until it becomes a claim.

## Verification

- `board::tests::reserve_blocked_task_and_claim_it_when_deps_finish`,
  `reserve_is_one_per_agent_and_lapses_when_the_reserver_is_busy`,
  `reservation_can_be_released_and_is_dropped_for_failed_workers`, `claim_waits_for_deps`.
- e2e: the fake worker reserves a blocked task when nothing else is claimable and continues a task it was
  handed. Whether that path runs depends on timing, so the board tests above are the guarantee.
