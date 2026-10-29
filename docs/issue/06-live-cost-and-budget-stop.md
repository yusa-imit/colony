# 06. Cost cap reacts 10+ minutes late

Kind: safety · Area: `stream`, new `cost`, `supervisor`, `worker`

## Problem

Cost was updated only from `result.total_cost_usd`, which arrives at turn end. With 12–20-minute turns (#03)
the reported total sat at $4.58 from +11 to +20 min while actual spend climbed from $19.90 to $44.31. A budget
stop then waited up to `grace_secs` (300 s) for in-flight turns. Issue #1 §1 measured overruns of +41% to +444%
depending on the cap.

## Fix

1. **Live accrual.** `stream` parses each assistant message's `id` and `usage`. The supervisor adds the
   input-side tokens of each new message (input, cache writes, cache reads) to the worker's in-flight estimate.
   Lines of one message repeat the same usage, so messages are counted once by id.
2. **Online calibration** (`cost::Meter`, pure). Every `result` gives the real cost delta of a turn and its exact
   usage. The meter keeps:
   - dollars per weighted token, with weights fitted on the run: input 1, 5m write 1.25, 1h write 2, read 0.05,
     output 5;
   - an output factor (total weighted tokens / input-side weighted tokens), because in-flight messages only
     carry their starting `output_tokens`.

   Before the first result it assumes $4/MTok and a factor of 1.3, which overestimates cheaper models, the safe
   direction. Total cost = settled cost from results + calibrated in-flight estimates, checked every second.
3. **Interrupt on a budget stop.** Working workers get
   `{"type":"control_request","request":{"subtype":"interrupt"}}` (verified: ends the turn within 50 ms), then
   their stdin is closed. The kill timer for a budget stop is capped at 30 s.

Expected overrun: about one API call per active worker.

## Verification

- `cost::tests::*`: weights, calibration recovering a known price, the in-flight estimate.
- `stream::tests::parses_assistant_usage_and_id`, `stream::tests::parses_result_usage`.
- `worker`: the interrupt line format, tested.
