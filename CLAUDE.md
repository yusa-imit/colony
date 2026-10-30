# colony (Rust)

A multi-agent harness in which several `claude` processes divide the work among themselves on a shared board.
The deliverable is a single binary. What it is, why, and the decisions behind it (D1–D11):
[`docs/design.md`](docs/design.md). Usage: [`README.md`](README.md).

## Rules

- **No AI trailers in commits or PRs** — neither `Co-Authored-By: Claude …` nor `🤖 Generated with …`. This
  applies to commits in this repository and to the commits colony has workers create (D11).
- The only runtime dependencies are `claude` and `git`. Do not introduce Node or any other runtime.
- `board`, `stream` and `prompts` perform no IO. Time and git verdicts are passed in as values.
- Workers talk to the harness over MCP (JSON-RPC over HTTP), because that is all Claude Code's MCP transports
  offer (D2).
- Record anything learned from running real claude workers in the field notes, `docs/design.md` §6. Most of the
  leniency in prompts and tool-input handling came from those runs; do not tighten it without evidence.
- Everything in this repository is written in professional English: docs, comments, user-facing strings, and
  commit messages.

## Verification

```sh
cargo fmt --check
cargo clippy --all-targets
cargo test        # unit + hub (git) + e2e (three fake claude workers, examples/fake_claude.rs)
```

CI runs the same commands on Linux and macOS (`.github/workflows/ci.yml`, clippy with `-D warnings`). Releases
are cut by pushing a version tag; see README "Releasing".

The e2e test runs the whole harness without an LLM; it must pass after any change to the supervisor, MCP or
hub. After changing worker-facing behavior (prompts, tools), run real claude once against a toy repository and
add the result to the field notes.
