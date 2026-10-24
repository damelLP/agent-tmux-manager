//! pi adapter for ATM.
//!
//! All pi-specific knowledge — the event vocabulary, the wire payload
//! shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. Symmetric with
//! `atm_claude_adapter`.
//!
//! Pi is a coding agent (<https://pi.dev/>, npm package
//! `@mariozechner/pi-coding-agent`) that exposes a TypeScript extension
//! API. ATM's pi adapter is a TypeScript extension (`pi-atm`,
//! tracked by bead `agent-tmux-manager-6dx`) that subscribes to pi's
//! `pi.on(eventName, handler)` events and forwards them to atmd over
//! the existing Unix socket. The Rust types in this crate describe the
//! wire payloads that extension produces and the translation into
//! `LifecycleEvent`.
//!
//! ## Event vocabulary
//!
//! 28 pi events. Captured: pi 0.73.1, 2026-10-24, pi's
//! `dist/core/extensions/types.d.ts`, which declares 29. The 29th,
//! `thinking_level_select`, is not modelled: it lands in
//! `PiEventType::Other`, and pi-atm doesn't forward it. At the
//! `agent-tmux-manager-9dn` spike (2026-05-02, pi version not recorded)
//! `tool_call` and `tool_result` were undeclared but real. See
//! `event::PiEventType` for the enumeration and `translate.rs` for
//! per-variant lifecycle mapping. Originally captured by the spike
//! under `extensions/pi-spike/`.
//!
//! ## Three-axis vendor model
//!
//! Pi is *provider-agnostic* — a single pi session can switch between
//! Anthropic, OpenAI, and other providers via `model_select`. The
//! three axes are:
//!
//! - **harness** = `pi`
//! - **provider** = `anthropic` / `openai-codex` / etc.
//! - **model** = `claude-sonnet-4-6` / `gpt-5.5` / etc.
//!
//! Provider+model changes ride [`atm_core::LifecycleEvent::ProviderModelChange`].
//!
//! ## NeedsInput is extension-mediated
//!
//! Pi has *no* dedicated permission-prompt event. Permission gating
//! happens *inside* an extension's `tool_call` handler via
//! `ctx.ui.select(...)`. ATM's pi adapter therefore registers a
//! `tool_call` handler and synthesizes
//! [`atm_core::NeedsInputReason::PermissionGate`] when the gate is
//! reached. A passive observer cannot detect this state; only an active
//! extension can.
//!
//! ## Captured payloads
//!
//! `tests/fixtures/pi_tool_run_0.73.1.jsonl` holds one real tool run, as
//! the frames pi-atm sent atmd. Captured: pi 0.73.1, 2026-10-24, pi's
//! TUI in a private tmux server with pi-atm loaded, frames tapped by
//! `socat -r` on the way to an isolated atmd, answered by a local fake
//! OpenAI-compatible provider (no model call). session_id absent:
//! pi-atm bug, queued. It covers every event pi-atm forwards except
//! `session_before_compact`, whose payload the adapter doesn't read.
//! The `atm_needs_input_*` events are pi-atm's own, not pi's.
//!
//! The hand-copied payloads in the `wire.rs` and `translate.rs` tests
//! come from the 2026-05-02 spike trace (pi version not recorded).
//! Tests marked `Synthetic:` use shapes no capture produced.
//!
//! ## Known gaps (pi 0.73.1)
//!
//! - `model_select` sends `{model, previousModel, source}`. `model` is
//!   pi's `Model` object, and the provider is `model.provider`. The
//!   adapter reads a top-level `provider`, so
//!   `ProviderModelChange.provider` is always `None`. Nothing reads it
//!   yet: `SessionDomain` keeps only the model.
//! - `model_select` fires only when the model changes (source `set`,
//!   `cycle` or `restore`). A fresh session sends none, so its model
//!   shows as unknown until the user switches.
//! - pi-atm sends no `session_id`. It reads
//!   `ctx.sessionManager.currentSessionId`, which pi 0.73.1 doesn't have
//!   (its session manager has `getSessionId()`), so atmd keys pi
//!   sessions by pid.

pub mod event;
pub mod translate;
pub mod wire;

pub use event::PiEventType;
pub use wire::RawPiEvent;
