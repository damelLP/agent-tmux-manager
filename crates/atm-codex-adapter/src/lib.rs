//! OpenAI Codex CLI adapter for ATM.
//!
//! All Codex-specific knowledge — the raw event vocabulary, the wire
//! payload shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. The daemon
//! (`atmd`) calls into the adapter at the connection boundary; nothing
//! in `atm-core` or `atm-protocol` references Codex.
//!
//! ## Relationship to the Claude adapter
//!
//! Codex's hook system is deliberately Claude-shaped: same stdin-JSON
//! transport, near-identical field names (`session_id`,
//! `hook_event_name`, `tool_name`, `tool_use_id`, ...). This crate
//! still shares **no code** with `atm-claude-adapter`: the similarity
//! is convergent evolution, not shared ancestry, and the two vendors
//! version and drift independently (the pi adapter needed a
//! schema-drift fix within a *single* vendor's releases; coupling two
//! vendors' parsers would double that blast radius). The module layout
//! intentionally mirrors `atm-claude-adapter` so the two crates stay
//! hand-diffable.
//!
//! ## Codex-specific semantics (validated against live codex-cli 0.146.1
//! traffic, 2026-08-10 spike)
//!
//! - Codex has a real `PermissionRequest` hook event — a vendor-signaled
//!   gate on an otherwise-normal tool — which maps to
//!   `NeedsInputReason::PermissionGate`. A silent, exit-0 hook is
//!   neutral: it neither allows nor denies (spike-verified — the
//!   approval dialog still renders with an observational hook attached).
//! - `PostToolUse` carries **no error signal**: `tool_response` is a
//!   plain string of raw output with no `is_error`/`success` field, so
//!   `ToolCallEnd.is_error` is always `false` for Codex.
//! - Codex has no status-line equivalent. Context usage is read
//!   defensively from the bounded tail of the rollout transcript path
//!   included in hook events; unavailable or changed transcript data
//!   never prevents the hook event itself from being processed.
//! - `PreToolUse` fires *before* `PermissionRequest` for the same gated
//!   call, and a `PreToolUse` may arrive with no matching `PostToolUse`
//!   (aborted call) — downstream state handles both orderings.
//!
//! ## Layers
//!
//! - [`event`] — `CodexEventType` enum (the 11 Codex hook event names)
//! - [`transcript`] — bounded, best-effort rollout token-usage reader
//! - [`wire`] — `RawCodexEvent` struct (deserialized JSON Codex sends
//!   on stdin to the hook script)
//! - [`translate`] — translation from raw event to `LifecycleEvent`

pub mod event;
pub mod transcript;
pub mod translate;
pub mod wire;

pub use event::CodexEventType;
pub use transcript::{read_token_usage, CodexTokenUsage};
pub use wire::RawCodexEvent;
