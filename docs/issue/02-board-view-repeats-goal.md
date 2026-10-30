# 02. `board_view` repeats the whole goal on every call

Kind: cost · Area: `board`, `prompts`

## Problem

`render_view` printed the full goal text as its first line. The protocol, the MCP `instructions`, the tool
description and the `task_complete` reply all tell workers to call `board_view`, so the goal was copied into
the context again and again, and every later API call in the session re-read every copy.

## Evidence

- Goal: 25,115 characters (a full design document).
- 38 `board_view` calls; each added **≈14.5k tokens** of context (p50; range 12.4k–17.2k).
- Cache write of those outputs plus their re-reads by later calls in the same session: **≈$12.7, about 20% of
  the run**.
- The toy runs had a 294-character goal, so this never showed.

Agensh renders the goal into the prompt once; `board_read` returns board entries only.

## Fix

- The goal moves into the system prompt (`# Goal` section of `prompts::system_prompt`). Every worker's system
  prompt is still identical, so the goal is cached once and shared by all workers and all fresh sessions (#05).
- `board_view` shows the goal's status and its first line only, with a pointer to the system prompt.
- The kickoff message no longer repeats the goal.

## Verification

- `board::tests::view_shows_goal_title_not_body`.
- `prompts::tests::system_prompt_carries_the_goal`.
- Field run: `board_view` results stay small regardless of goal size.
