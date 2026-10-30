# 03. `task_complete` tells workers to keep going in the same turn

Kind: root cause · Area: `mcp`, `prompts`

## Problem

The protocol says "End your turn when you finish a task", but the `task_complete` reply said
"**Pick your next task (board_view).**" The tool reply arrives last, and the models followed it.

The supervisor does everything at turn boundaries: it updates cost from `result`, hands out nudges with board
digests and mentions, decides whether to sleep, and paces concurrency. With one turn spanning several tasks, all
of that became up to 20 minutes coarse. This is the common cause of #1 §1 (cost cap reacts late) and §2
(context carried across tasks).

## Evidence

| Worker | Main turn | API calls | Cost | Tasks in the turn |
|---|---|---|---|---|
| w1 | 20.3 min | 113 | $8.94 | T1, T2, T3, T4, T21 |
| w2 | 18.5 min | 136 | $10.21 | T12, T15, T16, T19 |
| w3 | 19.3 min | 111 | $9.13 | T13, T11, T5, T17 |
| w4 | 17.7 min | 106 | $7.60 | T7, T6 |
| w5 | 18.6 min | 107 | $7.73 | T9, T14, T10 |
| w6 | 12.3 min | 70 | $6.20 | T8 (w6 ended its turn after T18 and T20, as the protocol asks) |

These six turns are $49.8, 80% of the run. In the toy runs, turns lasted 0.1–0.6 min, so a turn and a task
were the same thing.

## Fix

- The `task_complete` reply now says the task is done and to **end the turn now**, and that the next turn starts
  with a fresh digest. If the worker added no note since claiming the task, the reply also asks it to `note_add`
  anything reusable first (Agensh §B.1 sends the same reminder after a turn without a shared-context entry).
- Protocol: share notes (step 6) comes before `task_complete` (step 7), which ends the turn. "One task per turn"
  is stated as a rule.
- `board_view`'s description no longer says "the goal"; see #02.

This restores turn = task, which #05 (fresh session per task) and #06 (cost accounting) build on.

## Verification

- `board::tests::noted_since_claim_tracks_notes_after_the_claim`,
  `mcp::tests::complete_reply_ends_the_turn_and_asks_for_a_note_when_none_was_added`.
- Field run 4: every turn contained at most one `task_complete`, except the last worker's, which went on to
  call `goal_complete` right after the final task.
