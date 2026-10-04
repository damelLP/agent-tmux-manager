//! Claude Code adapter for ATM.
//!
//! All Claude-specific knowledge — the raw event vocabulary, the wire
//! payload shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. The daemon
//! (`atmd`) calls into the adapter at the connection boundary; nothing
//! in `atm-core` or `atm-protocol` references Claude.
//!
//! ## Layers
//!
//! - [`event`] — `ClaudeEventType` enum (the 12 Claude hook event names)
//! - [`wire`] — `RawHookEvent` struct (deserialized JSON Claude sends
//!   on stdin to the hook script)
//! - [`translate`] — translation from raw event to `LifecycleEvent`
//!
//! ## Agent teams (captured 2026-10-04)
//!
//! Real team runs with every hook logged: `--teammate-mode tmux` on
//! Claude Code 2.1.288, `-p --teammate-mode in-process` on 2.1.289.
//! Re-check before relying on these after an upgrade.
//!
//! - **tmux mode.** Each teammate is its own `claude` process (argv0
//!   `.../claude/versions/<version>`, comm `<version>`) with its own
//!   session id. Its argv carries `--agent-id <name>@<team>`,
//!   `--agent-name`, `--team-name`, `--agent-type` and
//!   `--parent-session-id <lead's full session id>`; atmd reads the last
//!   via `HarnessDefinition::parent_session_flag` to nest the teammate
//!   under its lead.
//! - Hooks from a tmux teammate's own session carry a top-level
//!   `agent_type` but no `agent_id`.
//! - `TeammateIdle` comes from the teammate's own session, with
//!   `teammate_name` set to its own name and `team_name`, and no
//!   `agent_id`. It therefore idles the session that sent it, never a
//!   named child (routing by name made a `waiter@<teammate>`
//!   placeholder).
//! - The lead gets no `SubagentStart` for tmux teammates. Its `Stop`
//!   lists them in `background_tasks` with `"type": "teammate"`. It did
//!   get `SubagentStop`s with `agent_type: ""` and agent ids matching no
//!   teammate.
//! - **in-process mode.** No `TeammateIdle` fired. The named teammate
//!   ran as a background subagent: `SubagentStart`/`SubagentStop` with
//!   `agent_id`, its own hooks carry that `agent_id`, and the `Agent`
//!   spawn response gives it as `agentId`. That build reported separately
//!   named teams as deprecated.
//! - Not captured: `TaskCreated`/`TaskCompleted`. Their `teammate_name`
//!   routing is still checked only against schema strings.

pub mod event;
pub mod translate;
pub mod wire;

pub use event::ClaudeEventType;
pub use wire::RawHookEvent;
