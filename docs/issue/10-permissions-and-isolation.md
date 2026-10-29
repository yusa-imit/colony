# 10. Allow list denies harmless forms, file tools are unscoped, the launching session leaks in

Kind: isolation · Area: `config`, `worker`, `hub`, `prompts`

## Problem and evidence

Agensh workers run inside containers and may use "whatever is at hand". colony runs workers on the host, with
Claude Code's allow list as the only boundary, and that boundary was wrong in both directions.

1. **False denials.** 41 permission denials across all six workers: `cd <dir> && …` (19), redirection or heredoc
   into a file (10), env prefixes (2), `sed -i` (2), brace expansion (2), and others. The denial text says
   "do not retry", which pushes workers to give up on a step.
2. **False allows.** The bare `Write`/`Edit` allow rules are not path-scoped: 30 file writes landed in `/tmp`
   (w2, w3, w5).
3. **Full tool surface.** Every worker got 32 built-in tools, including `PushNotification`, `RemoteTrigger`,
   `WebFetch`, `CronCreate`, `SendMessage` and `ShareOnboardingGuide`. Loading the colony tools also cost a
   `ToolSearch` round trip per session (#13).
4. **Environment leakage.** Workers inherited the launching Claude Code session's variables:
   - `CLAUDE_CODE_PLUGIN_DIRS` loaded the terminal's inline plugin into every worker, despite
     `--setting-sources`;
   - `CLAUDE_CODE_MESSAGING_SOCKET/TOKEN` gave workers a socket to the parent session (the init event shows
     `messaging_socket_path`);
   - `CLAUDE_EFFORT` set the workers' effort without `--effort`.

## Fix

- **File tools scoped to the clone.** The defaults drop bare `Write`/`Edit`; each worker gets `Edit(//<clone>/**)`
  and `Write(//<clone>/**)`. Verified: inside passes, outside is denied, in both `acceptEdits` and `manual` mode.
- **Allow list.** Adds `Bash(cd *)` (verified with `cd sub && ls`), `Bash(sed *)` and
  `Bash(./node_modules/.bin/*)`.
- **Protocol** (worker prompt):
  - write files with Write/Edit, not shell redirection;
  - delete with `git rm` / `git clean -f <path>`;
  - put scratch files in `.colony-tmp/` inside the clone, which is excluded through `.git/info/exclude`;
  - writes outside the clone are denied.
- **Explicit tool set.** New `tools` setting, default `Bash, Read, Edit, Write, Glob, Grep`, passed as `--tools`.
  Verified: MCP tools then load up front without `ToolSearch`.
- **Environment.** Workers start without the launching session's Claude Code variables: `CLAUDECODE`,
  `CLAUDE_PID`, `CLAUDE_EFFORT`, `CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_EXECPATH`, `CLAUDE_CODE_SSE_PORT`, and
  the `CLAUDE_CODE_PLUGIN*`, `CLAUDE_CODE_MESSAGING*`, `CLAUDE_CODE_SESSION*` and `CLAUDE_CODE_CHILD*` families.
  This is a deny list on purpose: an allow list would break user-chosen authentication, proxies and toolchain
  variables.
- **Optional Bash sandbox** (`sandbox = true`). Uses Claude Code's sandbox:
  - `enabled`, `autoAllowBashIfSandboxed`, `failIfUnavailable`;
  - `filesystem.allowWrite`: the hub plus `sandbox_allow_write`;
  - `network.allowedDomains`: `127.0.0.1`, `localhost` plus `sandbox_allowed_domains`.

  Verified: Bash runs without allow-list friction, the gate is reachable and `git push` to the hub works.

  Off by default, because it is machine-specific: package stores outside the clone and registries need explicit
  entries, and temp dirs stay writable.

## Verification

- `worker::tests::*`: scoped rules, the `--tools` list, sandbox settings and session-variable stripping.
- `hub` test: the clone excludes `.colony-tmp/`.
- Field run: no `ToolSearch` calls, no denials for `cd … &&`, no writes outside the clone.
