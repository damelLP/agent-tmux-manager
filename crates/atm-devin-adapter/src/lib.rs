//! Devin CLI adapter for ATM.
//!
//! All Devin-specific knowledge — the raw event vocabulary, the wire
//! payload shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. The daemon
//! (`atmd`) calls into the adapter at the connection boundary; nothing
//! in `atm-core` or `atm-protocol` references Devin CLI.
//!
//! Devin CLI's hook system
//! (<https://docs.devin.ai/cli/extensibility/hooks/overview>) is
//! structurally close to Claude Code's — JSON-on-stdin, a
//! `session_id` correlation id, the same `command`-hook config
//! shape — which is why this crate mirrors `atm-claude-adapter`'s
//! layout closely. The event vocabulary and tool-name casing differ
//! (Devin uses lowercase/`snake_case` tool names), so it gets its own
//! adapter rather than reusing Claude's.
//!
//! ## Layers
//!
//! - [`event`] — `DevinEventType` enum (the 8 Devin CLI hook event names)
//! - [`wire`] — `RawDevinHookEvent` struct (deserialized JSON Devin
//!   sends on stdin to the hook script)
//! - [`translate`] — translation from raw event to `LifecycleEvent`

pub mod event;
pub mod translate;
pub mod wire;

pub use event::DevinEventType;
pub use wire::RawDevinHookEvent;
