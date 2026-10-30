# 13. Workspaces left behind, model not shown, ToolSearch round trip

Kind: housekeeping · Area: `run`, `main`, `supervisor`

## Problem and evidence

1. `ws/` stayed after the run: 2.2 GB (6 clones and the verification clone, ≈310 MB each with `node_modules`).
2. Every session started with a `ToolSearch` call to load the deferred `mcp__colony__*` tools (6 calls).
3. No `--model` was given, so every worker ran the account's default (Opus). The start banner did not say so.

## Fix

1. After a run that ends with the goal complete and the branch exported, `ws/` is removed. `hub.git`,
   `board.json`, `summary.json`, `tools.jsonl` and `agents/` are kept. `keep_workspaces = true` disables this.
   `colony clean` removes `ws/` from every finished run (one with `summary.json`) under the state directory.
2. Fixed by the explicit `--tools` list (#10). Verified: with `ToolSearch` absent, MCP tools load up front.
3. The banner shows the model setting ("claude default" when unset). The supervisor logs the resolved model from
   the first `system/init` and puts it in `RunStatus` and on the dashboard.

## Verification

- e2e: `ws/` is gone after a completed run, and the kept files are present.
- `run::tests::clean_removes_only_finished_workspaces`.
