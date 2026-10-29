# 11. One-shot plan with a serial head and tail; internal deps missed

Kind: coordination · Area: `prompts`, `board`

## Problem

Agensh has no planning phase: each worker proposes and claims its own sub-task. colony seeds a planning task T1
(D8, to prevent duplicate plans), so one worker decomposes the whole goal at once, and the plan's shape decides
how parallel the run can be. The planning instructions said nothing about the critical path or about
dependencies between bundles.

## Evidence

- w1 proposed all 21 tasks 38 seconds after claiming T1. No other `task_propose` happened in the run.
- **Serial head.** Every task depended on the skeleton T2 (3.2 min); 19 waiting turns ($2.54).
- **Serial tail.** T22 (documentation, manual checks, verify) depended on all 19 implementation tasks. Nothing of it
  could start early: 13 waiting turns ($2.77) and two README drafts (#09).
- **Internal dependency missed.** One category was split alphabetically, and T12 needed a helper
  that the split had put in T13. Both workers wrote it (#07).

## Fix

The planning bullet of the protocol and the T1 detail now ask the planner to:

- keep the critical path short: make the shared skeleton as small as possible and split wrap-up work so the
  parts that do not need everything (docs skeletons, checks) can start early;
- put helpers that several bundles need into one early task and make the others depend on it, rather than
  splitting by name alone;
- state each task's acceptance check in its detail.

The protocol also reminds every worker that proposing missing tasks is part of the loop, not only the planner's
job (Agensh: workers propose their own sub-tasks).

## Verification

- `prompts::tests::system_prompt_mentions_verify_and_planning_rules`.
- Field run 4 (Haiku): the planner proposed the README and final-check tasks without deps, noticed, and said so
  in chat. The guidance alone did not prevent it. Reservations (#09) limit the damage, because a worker can
  hold the wrap-up task's intent early. If this recurs on larger goals, the next step is a board-side check
  that flags wrap-up-like tasks with no deps when they are proposed.
