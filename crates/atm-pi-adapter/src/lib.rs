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
//! `ctx.ui.select(...)`. pi-atm therefore wraps `ctx.ui.select` and
//! emits the synthetic `atm_needs_input_open` / `atm_needs_input_resolved`
//! events around the dialog. They translate to
//! [`atm_core::NeedsInputReason::Notification`] (kind `PermissionPrompt`)
//! and back to `WorkingStart`. The adapter still maps a `tool_call`
//! with `needs_user_input: true` to
//! [`atm_core::NeedsInputReason::PermissionGate`], but no pi-atm version
//! sends that flag. A passive observer cannot detect this state; only
//! an active extension can.
//!
//! ## Captured payloads
//!
//! `tests/fixtures/pi_tool_run_0.73.1.jsonl` holds one real tool run, as
//! the frames pi-atm sent atmd. Captured: pi 0.73.1, 2026-10-24, pi's
//! TUI in a private tmux server with pi-atm installed by `atm setup`,
//! frames tapped by `socat -r` on the way to an isolated atmd, answered
//! by a local fake OpenAI-compatible provider (no model call). It covers
//! every event pi-atm forwards except `session_before_compact`, whose
//! payload the adapter doesn't read. The `atm_needs_input_*` events are
//! pi-atm's own, not pi's.
//!
//! `tests/fixtures/pi_new_session_0.73.1.jsonl` holds `/new` in one pi
//! process. Captured: pi 0.73.1, 2026-10-24, set up the same way, with
//! startup, `/new` and `/quit` and no prompt. pi loads extensions afresh
//! for each session, so the old pi-atm instance sent `session_start` and
//! `session_shutdown` (reason `new`) on one connection, and the new
//! instance sent the new session's frames on a second one.
//!
//! The hand-copied JSON payloads in the `wire.rs` and `translate.rs`
//! tests follow the spike trace. Captured: pi, version not recorded,
//! 2026-05-02 (trace
//! `/tmp/atm-pi-spike-pid1226148-2026-05-02T08-55-56-043Z.jsonl`, not
//! kept), pi-spike tracer in an interactive session, spike
//! `agent-tmux-manager-9dn`; added in 7a20d86 on 2026-05-10.
//!
//! Tests that build `PiPayload` structs check mapping logic and claim
//! no capture. `Synthetic:` marks the tests and examples whose shapes
//! or values no capture produced.
//!
//! ## Known gaps (pi 0.73.1)
//!
//! - `model_select` sends `{model, previousModel, source}`. `model` is
//!   pi's `Model` object, and the provider is `model.provider`. The
//!   adapter reads a top-level `provider`, so
//!   `ProviderModelChange.provider` is always `None`. Nothing reads it
//!   yet: `SessionDomain` keeps only the model.
//! - `model_select` fires only when the user changes model: pi's source
//!   emits it for `set` and `cycle`, never for the declared `restore`.
//!   A fresh or resumed session sends none, so its model shows as
//!   unknown until the user switches.
//! - `model_select`'s `model.contextWindow` isn't passed on.
//!   `ContextUpdate.context_window_size` is always `None`, so context
//!   percentages use the unknown model's 200K default.
//! - Each assistant message's `usage` and `cost` cover that request
//!   only. The adapter takes the latest from each `context` event, and
//!   atm-core stores it as the session's cost, so a pi session shows
//!   one request's cost rather than its total. A turn's last reply
//!   arrives only in `agent_end.messages`, which the adapter doesn't
//!   read, so the figure also lags one request behind.
//! - `input` becomes `PromptSubmit` only for source `interactive`.
//!   Prompts with source `rpc` or `extension` are dropped.
//! - Two pi processes can hold one session, because pi takes no lock on
//!   session files. Checked: `pi -c` in a second pane, and `--session`
//!   on one file; `/resume` of an open session is unverified. Both
//!   processes send the same `session_id`. atmd keeps a row per pid
//!   (`atm list` showed both), but removing either pid un-indexes the id
//!   (`handle_remove_by_pid`, atmd `registry/actor.rs:996`), and the TUI
//!   keys rows by `SessionId` (atm-tui `app.rs:55`), so the two collapse
//!   into one. Claude has the same case with `claude --resume`. Queued as
//!   "one session id held by two pids". Older pi-atm sent no
//!   `session_id` (it read `currentSessionId`, which no pi release has),
//!   so each pid got its own `pending-<pid>` and the case never arose.
//! - Discovery can't see pi. pi sets `process.title = "pi"` (found in
//!   0.50.0, 0.65.0, 0.70.0, 0.72.1 and 0.73.1, not in 0.10.0), so
//!   `/proc` shows cmdline `pi` and exe `node`. pi's matchers need a
//!   path, and pi rejects bare cmdline matches. A pi already running
//!   when atmd starts appears only with pi-atm's next event.
//! - `/new`, `/resume`, `/fork` and `/reload` replace the session in the
//!   same process. pi loads pi-atm afresh, so `session_shutdown` comes
//!   from the old instance and `session_start` from the new one on a
//!   new connection. atmd ends the old row and starts a new one. That
//!   includes `/reload`, which keeps the id, so the row's model and cost
//!   reset (fake-b at $0.0033 became Unknown at $0). The old instance's
//!   connection stays open until pi exits. Over rpc, `new_session` and
//!   `clone` fired `session_start` twice; the TUI's `/new` fired it once.

pub mod event;
pub mod translate;
pub mod wire;

pub use event::PiEventType;
pub use wire::RawPiEvent;
