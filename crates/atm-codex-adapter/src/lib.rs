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
//! ## Codex-specific semantics
//!
//! Captured: codex-cli 0.146.1, 2026-08-10, the spike's `codex exec`
//! and interactive tmux sessions (logs not kept); added in 49f2350 on
//! 2026-08-16. Re-checked where noted against codex-cli 0.163.0 on
//! 2026-10-24, with the capture under "Captured payloads".
//!
//! - Codex has a real `PermissionRequest` hook event — a vendor-signaled
//!   gate on an otherwise-normal tool — which maps to
//!   `NeedsInputReason::PermissionGate`. A silent, exit-0 hook is
//!   neutral: it neither allows nor denies (spike-verified — the
//!   approval dialog still renders with an observational hook attached).
//!   Re-checked on 0.163.0.
//! - `PostToolUse` carries **no error signal**: `tool_response` is a
//!   plain string of raw output with no `is_error`/`success` field, so
//!   `ToolCallEnd.is_error` is always `false` for Codex. The spike checked
//!   a failing call; 0.163.0 was checked with succeeding calls only.
//! - Codex has no status-line equivalent. Context usage is read
//!   defensively from the bounded tail of the rollout transcript path
//!   included in hook events; unavailable or changed transcript data
//!   never prevents the hook event itself from being processed. The
//!   rollout reader was added in 49f2350 on 2026-08-16, codex version
//!   not recorded; its `token_count` fields are unchanged in 0.163.0.
//! - `PreToolUse` fires *before* `PermissionRequest` for the same gated
//!   call (re-checked on 0.163.0), and a `PreToolUse` may arrive with no
//!   matching `PostToolUse` (aborted call, not re-checked) — downstream
//!   state handles both orderings.
//! - Process discovery: a shared app-server daemon runs the same `codex`
//!   binary as `codex app-server --listen unix:// --managed-daemon` and
//!   `codex app-server daemon pid-update-loop` (survives
//!   `codex app-server daemon stop`, may be reparented to init).
//!   Sandboxed tool calls run the same binary as `codex-linux-sandbox`
//!   inside `bwrap` (whose argv names the codex binary). None are
//!   sessions; the codex `HarnessDefinition` in `atm-core` excludes them
//!   via `process_excludes`. Captured: codex-cli 0.160.0, 2026-10-03, a
//!   live process listing; added in dd5b1b7. On 0.163.0 the daemon's
//!   argv also carries `--analytics-default-enabled`, and an isolated
//!   atmd's startup discovery listed the TUI but not the daemon.
//!
//! ## Captured payloads
//!
//! `tests/fixtures/codex_tui_session_0.163.0.jsonl` holds one TUI
//! session's hook payloads, one per line, as codex wrote them to the
//! hook's stdin. Captured: codex-cli 0.163.0, 2026-10-24, codex's TUI in
//! its default shared-daemon mode in a private tmux server, inside a
//! network namespace with only loopback, with an isolated HOME and
//! CODEX_HOME, answered by a local fake Responses-API provider (no model
//! call); a logging hook was registered for all 11 events in the
//! `hooks.json` shape `atm setup` writes; the CODEX_HOME and working
//! directory prefixes were rewritten to `/home/user/.codex` and
//! `/home/user/project`. One prompt ran a shell command the fake model
//! asked to escalate (`-a on-request`), approved in the dialog, then
//! `/compact` and `/quit`. `SessionEnd` fired only when the daemon was
//! stopped (see Known gaps). `codex --no-daemon` gives the same payload
//! shapes. Codex sends no `pid` or `tmux_pane`; `atm-codex-hook` adds
//! them.
//!
//! `tests/fixtures/codex_rollout_tokens_0.163.0.jsonl` is that session's
//! rollout, trimmed to its `task_started`, `task_complete`,
//! `token_usage_record` and `token_count` records, lines kept verbatim.
//! The usage numbers are the fake provider's, except in the last
//! `token_count`: after `/compact` codex writes a `last_token_usage`
//! with only `total_tokens` set, its own estimate of the compacted
//! history.
//!
//! The JSON payloads in the `wire.rs` tests are trimmed copies of the
//! spike's logs. Captured: codex-cli 0.146.1, 2026-08-10, the spike
//! described above (`spike-log*.jsonl`, not kept); added in 49f2350 on
//! 2026-08-16.
//!
//! Tests that build `RawCodexEvent` structs check mapping logic and
//! claim no capture. `Synthetic:` marks the tests whose payloads no
//! capture produced.
//!
//! ## Known gaps (codex-cli 0.163.0)
//!
//! - **Hooks run under a shared app-server daemon.** By default the TUI
//!   runs its sessions on one shared `codex app-server --managed-daemon`
//!   process, and codex runs every hook as that daemon's child. Checked
//!   with two TUIs in two tmux panes, the real `atm-codex-hook` and an
//!   isolated atmd:
//!   - every session's hooks report the daemon's pid, so atmd, which
//!     keys sessions by pid, folds all codex sessions into one row, and
//!     discovery's `pending-<pid>` row for each TUI never reconciles;
//!   - `TMUX_PANE` in the hook's environment is the daemon's, inherited
//!     from whichever TUI started it, so other sessions show that pane;
//!   - `/quit` detaches ("Any running work continues") and fires no
//!     `SessionEnd`; it arrives when the daemon stops.
//!
//!   `codex --no-daemon` restores one process per session: each hook
//!   sees its own TUI's pid and pane, and `/quit` fires `SessionEnd`
//!   (checked with two TUIs). `--dangerously-bypass-hook-trust` also
//!   forces this embedded mode. Nothing in the hook's environment names
//!   the client TUI. Fix pending, a user decision. The TUI's
//!   `--no-daemon` flag is absent from codex-cli 0.155.0 and present in
//!   0.158.0 (static check of the npm binaries, not run); that the
//!   daemon is the default is checked on 0.163.0 only.
//! - **Usage lags one request mid-turn.** Codex writes a request's
//!   `token_count` after the tool output it led to, so hooks during a
//!   turn see the previous request's usage. By `Stop` the turn's last
//!   request is on disk. The `token_usage_record` codex writes as each
//!   reply completes is not read. Not checked on 0.146.1.
//! - No `info: null` `token_count` placeholders appeared: 0 of 40 across
//!   `codex exec` and TUI runs. [`read_token_usage`] still skips them.
//! - `SubagentStart` and `SubagentStop` have never been captured.
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
