# 07. No mid-turn delivery of messages and notes

Kind: coordination · Area: `board`, `prompts`, `server`, new `inbox`, `worker`

## Problem

Agensh delivers direct messages and new shared-context entries mid-turn, appended to the result of the worker's
next infrastructure tool call (§B.2). colony delivered them only at turn start (nudge) or when the worker
happened to call `board_view`/`chat_read`. With 20-minute turns (#03), and even with one-task turns, a worker
coding for several minutes was deaf.

## Evidence

At 4.7 min w2 wrote "@w3 I'll take the shared helper in T12 — please leave it out of T13". w3 made no MCP call
until it finished T13. It read the message at 6.8 min ("I saw your message late"), after its own copy of the helper
was already merged. w2 then threw away its own copy of the helper, its test and a shared test-input file during the
rebase.

## Fix

- **Inbox in the board** (pure). For each agent the board tracks what it has been delivered:
  - direct `@wNN` mentions from others;
  - notes added by others.

  `take_inbox(agent)` returns everything new and advances the markers, so each item is delivered once.
  `read_chat` also advances the mention marker.
- **Delivery after every tool call.** Each worker gets a `PostToolUse` hook (via `--settings`, matcher `*`)
  that runs `colony inbox`. That command asks `GET /inbox?agent=wNN` for pending items and prints them as
  `hookSpecificOutput.additionalContext`. With nothing pending it prints nothing, and it always exits 0 within
  a short timeout. Verified on claude 2.1.312: the text reaches the model, also under
  `--setting-sources project,local`.
- **Nudges use the same inbox**, so a mention shown mid-turn is not repeated at the next turn.
- `deliver_mid_turn` setting (default `true`).

### Considered and not used

Writing a user message to the worker's stdin during a turn also works: the CLI absorbs it at the next tool
boundary (verified). But a message that races the end of a turn starts an extra turn the supervisor did not ask
for. The hook runs only inside a turn, so it has no such race.

## Verification

- `board::tests::inbox_delivers_direct_mentions_and_notes_once`.
- `prompts::tests::renders_inbox`, `inbox::tests::hook_output_*`.
