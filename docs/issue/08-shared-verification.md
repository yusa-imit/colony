# 08. Every idle worker re-runs the full verification

Kind: waste · Area: `mcp`, `server`, `prompts`

## Problem

Only `goal_complete` ran the configured `verify` command. A worker that wanted to know whether colony-main was
healthy had to run the chain itself, and the IdleCheck nudge asked every idle worker to do exactly that (#04).
Agensh's redundancy is a feature under its time budget. Under a cost cap it is just repeated spend.

## Evidence

In the last four minutes, with every implementation task done, these overlapping checks ran:
- w4 build (21.8 min);
- w2 a manual check + build (22.3 min);
- w6 black-box review of T21 (23.3 min);
- w5 full verify chain (23.6 min);
- w4 an audit of the goal's completion criteria (25.7 min);
- the T22 owner's full run;
- the harness's own `verify`.

## Fix

- New worker tool **`verify`**: runs the configured `verify` command on the current colony-main in the
  harness's verification clone, through the gate, and returns pass/fail with the output tail.
- Results are cached per colony-main commit. A second `verify` (from anyone) on the same commit returns the
  cached result, with who ran it and when, at no cost.
- `goal_complete` reuses a passing cached result for the current head instead of running again.
- Without a configured `verify`, the tool says so and the worker checks by hand as before.
- The protocol and the IdleCheck nudge point at `verify` instead of "run the checks".

## Verification

- `mcp::tests::verify_cache_hits_only_on_the_same_head`.
- e2e: configured with a `verify` command; the fake worker calls `verify` before `goal_complete`, and the hub's
  verify runs once per head.
