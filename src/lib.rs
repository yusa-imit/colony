//! colony — a multi-agent harness in which several `claude` processes divide the work among
//! themselves on a shared board, with no central orchestrator. Structure and decisions: `docs/design.md`.
//!
//! ```text
//! board       task / chat / note / goal state machine (no IO)
//! stream      one claude stream-json line → event (no IO)
//! prompts     worker protocol, nudge and inbox text (no IO)
//! cost        live cost estimate calibrated against turn results (no IO)
//! config      defaults ← colony.toml ← CLI
//! hub         bare hub repo, worker clones, merge checks, result export (git)
//! gate        concurrency limit for heavy commands
//! inbox       `colony inbox`: mid-turn delivery through a PostToolUse hook
//! mcp         worker tools (JSON-RPC)
//! server      HTTP: /mcp · /gate/hold · /inbox · /api/state · dashboard
//! worker      one claude process
//! supervisor  turn slots, usage-limit handling, wake-ups, shutdown
//! run         composition root
//! ```

pub mod board;
pub mod config;
pub mod cost;
pub mod gate;
pub mod hub;
pub mod inbox;
pub mod mcp;
pub mod prompts;
pub mod run;
pub mod server;
pub mod stream;
pub mod supervisor;
pub mod worker;
