# 05. Context carried across tasks

Kind: cost · Area: `supervisor`, `prompts`, `config`

## Problem

D5 keeps each worker's process (and session) alive for the whole run. Within a task that keeps the prompt cache
warm. Across tasks, every API call of task N re-reads the transcript of tasks 1..N-1. The model ran with a 1M
context window and never compacted. Agensh ran Copilot with a 272k input cap, which bounded the same effect.

## Evidence

- Context per call: p50 185k, p90 317k, max 411k tokens. The kickoff is ≈45k after the first `board_view`.
- Each task started on top of its predecessors: w1's T21 started at 272k, w2's T22 at 382k.
- Cache reads were half the spend: $31.26 of $62.43.
- Context carried in from earlier tasks: 73.5M tokens, ≈$14.7, measured per task segment. Issue #1's upper bound
  is $17.8. These overlap with #02's figure.

## Fix

- New `fresh_session` setting (default `true`). When a turn ends, the worker holds no claim and has completed
  at least one task in its current session, the supervisor recycles it:
  1. close stdin and wait for the exit (killed after 30 s);
  2. start the process again under a new session id;
  3. send a fresh kickoff (identity, clone, time left, rules reminder) followed by the usual digest.
- The board, notes, task logs and git already carry everything the next task needs. Notes and task logs are the
  shared context Agensh relies on.
- Restarts after crashes still `--resume` the same session (D5).
- The goal lives in the shared system prompt (#02), so a fresh session costs one cached prefix read plus a short
  kickoff.

## Verification

- e2e: the fake claude is restarted between tasks (new `--session-id`), and the run completes.
- `supervisor::tests::decide_*` covers when recycling happens.
- Field run: the context at each task start stays near the kickoff size.
