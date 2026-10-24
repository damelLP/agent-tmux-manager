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
//! atm-core keeps only the model: `SessionDomain` has no provider.
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
//! by a local fake OpenAI-compatible provider (no model call). pi started
//! on `fake-a` (window 128000), and Ctrl+P switched it to `fake-b`
//! (32000). It covers every event pi-atm forwards except
//! `session_before_compact`, whose payload the adapter doesn't read. The
//! `atm_needs_input_*` events are pi-atm's own, not pi's. This pi-atm
//! (#76's first commit) forwarded `model_select`'s models whole, so the
//! frames show what pi 0.73.1 itself puts on a Model.
//!
//! `tests/fixtures/pi_new_session_0.73.1.jsonl` holds `/new` in one pi
//! process. Captured: pi 0.73.1, 2026-10-24, set up the same way, with
//! startup, `/new` and `/quit` and no prompt. pi loads extensions afresh
//! for each session, so the old pi-atm instance sent `session_start` and
//! `session_shutdown` (reason `new`) on one connection, and the new
//! instance sent the new session's frames on a second one.
//!
//! `tests/fixtures/pi_session_cost_0.73.1.jsonl` holds two prompts on
//! fake-b ($3/$15 per M): a read tool call and its reply, then one more
//! reply. Captured: pi 0.73.1, 2026-10-24, set up the same way, starting
//! on fake-b, with pi-atm's usage snapshot. pi's footer showed `$0.007`
//! after the first prompt and `$0.011` after the second.
//!
//! `tests/fixtures/pi_model_select_0.62.0.jsonl` holds a model switch on
//! an older pi. Captured: pi 0.62.0, 2026-10-24, set up the same way,
//! with Ctrl+P and no prompt, and pi-atm picking `{id, provider,
//! contextWindow}` from `model_select`'s `model` and `previousModel`.
//!
//! In every capture the provider sets a probe header and `authHeader`
//! with a fake key. pi 0.62.0 resolves both onto the Model object
//! (`Authorization: Bearer <apiKey>`), and so do 0.43.0, 0.50.0 and
//! 0.60.0 (read in their `model-registry.js`, not run). pi-atm used to
//! forward `model_select`'s models whole, headers included; on pi 0.62.0
//! that sent the key to atmd. pi 0.63.0, the next release, and the later
//! ones read (0.64.0, 0.65.0, 0.70.0, 0.72.1, 0.73.1) keep models.json and
//! `registerProvider` headers off the Model and resolve them per request.
//! Built-in github-copilot models still carry static, non-secret headers.
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
//! ## Cost and tokens
//!
//! pi prices each reply on its own: an assistant message's `usage`
//! covers that request only. pi-atm puts a `usage` snapshot on `context`
//! and `turn_end` frames, summed over every assistant message in the
//! session file as pi's footer sums them, so atm shows the cost pi shows.
//! `context` fires before each request and `turn_end` after each reply is
//! saved, so a prompt's last reply counts before `agent_end`. The
//! snapshot's tokens are input + output + cacheRead + cacheWrite, and its
//! active context is `ctx.getContextUsage().tokens`, which the footer's
//! percentage uses. pi before 0.49 has no `getContextUsage()`, so pi-atm
//! counts the last reply on the current branch as pi's
//! `calculateContextTokens` does, which is what those versions' footers
//! show. Checked: pi 0.73.1, 0.50.0 and 0.48.0, two prompts and three
//! priced requests. atm and the footer agreed after each prompt:
//! $0.006975 and $0.01065 against `$0.007` and `$0.011`, at `3.8%`.
//!
//! pi's `/session` (`get_session_stats`) sums `state.messages` instead:
//! the current branch since the last compaction. After a compaction or a
//! /tree branch switch it shows less than the footer, whose sum covers
//! every branch in the file. atm follows the footer, which is always on
//! screen. Read in pi 0.73.1's source, not run.
//!
//! ## Known gaps (pi 0.73.1)
//!
//! - `model_select` fires only when the user changes model: pi's source
//!   emits it for `set` and `cycle`, never for the declared `restore`.
//!   A fresh or resumed session sends none, so its model shows as
//!   unknown until the user switches. pi-atm's envelope snapshot of
//!   `ctx.model` holds the startup model, but the adapter reads only its
//!   context window.
//! - The context window comes from that snapshot, on `context` events.
//!   pi-atm before the snapshot sends none, so context percentages use
//!   the unknown model's 200K default until the user re-runs `atm setup`.
//! - After a model switch, the row pairs the new model with the old
//!   model's window until the next `context` event, which comes with the
//!   next prompt. `model_select` carries the new window, but
//!   `ProviderModelChange` has no field for it, and adding one is an
//!   atm-core change. Checked: one prompt on fake-a, then Ctrl+P, and
//!   `atm list` showed fake-b at `0.8% (1.0K/128K)`.
//! - pi-atm before the usage snapshot sends no totals, until the user
//!   re-runs `atm setup`. The adapter then takes the latest reply in each
//!   `context` event, so the row shows one request's cost rather than
//!   the total, a reply behind. Checked: main's
//!   pi-atm against this atmd showed $0.0033, then $0.003675, where the
//!   footer showed `$0.007` and `$0.011`.
//! - After `/resume`, `/fork` or `/reload`, the new row shows $0 until the
//!   next prompt's first `context` frame, while pi's footer shows the
//!   session file's total at once. It shares a cause with the
//!   `model_select` gap above: one pi event maps to one `LifecycleEvent`,
//!   so `session_start` can't also carry the usage or the model.
//!   Unverified: read, not run.
//! - Compaction and branch-summary calls aren't counted. pi's footer
//!   doesn't count them either, since they aren't assistant messages.
//!   Read, not run.
//! - After a compaction, pi shows the context size as `?` until the next
//!   reply. pi-atm then sends the cost only, and the row keeps its last
//!   context size. Unverified.
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
//!   same process. pi (0.65.0 and later) loads pi-atm afresh, so
//!   `session_shutdown` comes from the old instance and `session_start`
//!   from the new one on a new connection. atmd ends the old row and
//!   starts a new one. That includes `/reload`, which keeps the id, so
//!   the row's model and cost reset (fake-b at $0.0033 became Unknown at
//!   $0). The old instance's connection stays open until atmd closes it
//!   after 300 s idle. Over rpc, `new_session` and `clone` fired
//!   `session_start` twice; the TUI's `/new` fired it once.
//! - atmd renames a row only from `pending-<pid>` to a real id. So if the
//!   old instance's `session_shutdown` is lost (pi-atm drops sends during
//!   its 1 s reconnect back-off), the new session's events land on the
//!   old row, which keeps the old id. And if it arrives after the new
//!   `session_start` (both instances connect afresh when atmd has closed
//!   the idle connection), it removes the new session's row until that
//!   session's next event. Both unverified; queued as atmd registry items.
//! - pi 0.50.0 to 0.64.0 switch sessions in place: one pi-atm instance
//!   throughout, and only `session_switch`, which pi-atm doesn't forward.
//!   pi-atm reads the id on every event, so its frames follow the switch,
//!   but atmd keeps the row's first id (the rename rule above). Checked
//!   with pi 0.64.0 over rpc: after `new_session`, frames carried the new
//!   id and `atm list` still showed the first.

pub mod event;
pub mod translate;
pub mod wire;

pub use event::PiEventType;
pub use wire::RawPiEvent;
