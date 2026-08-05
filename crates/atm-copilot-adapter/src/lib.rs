//! GitHub Copilot CLI adapter for ATM.
//!
//! All Copilot-specific knowledge — the raw event vocabulary, the
//! wire payload shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. The daemon
//! (`atmd`) calls into the adapter at the connection boundary; nothing
//! in `atm-core` or `atm-protocol` references Copilot CLI.
//!
//! See <https://docs.github.com/en/copilot/reference/hooks-reference>
//! for Copilot CLI's hook system. Unlike Claude Code and Devin CLI,
//! Copilot's hook configuration and event payloads use `camelCase`.
//!
//! ## Layers
//!
//! - [`event`] — `CopilotEventType` enum (the hook event names)
//! - [`wire`] — `RawCopilotHookEvent` struct (deserialized JSON
//!   Copilot sends on stdin to the hook script, plus fields injected
//!   by `atm-copilot-hook`)
//! - [`translate`] — translation from raw event to `LifecycleEvent`

pub mod event;
pub mod translate;
pub mod wire;

pub use event::CopilotEventType;
pub use wire::RawCopilotHookEvent;
