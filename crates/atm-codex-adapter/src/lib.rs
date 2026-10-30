//! OpenAI Codex CLI adapter for ATM.
//!
//! All Codex-specific knowledge — the raw event vocabulary, the wire
//! payload shape, the rollout transcript's token-usage records, and the
//! translation into vendor-neutral `atm_core::LifecycleEvent` — lives in
//! this crate. The daemon (`atmd`) calls into the adapter at the
//! connection boundary.
//! `atm-core` holds Codex's built-in harness metadata and `atm-protocol`
//! its `CodexEvent` wire envelope, two exceptions documented in the
//! Architecture section of the repository's AGENTS.md.
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
//! with every field 0 except `total_tokens`, its own estimate of the
//! compacted history. Codex's thread-title request runs in a separate
//! thread (`thread_source: thread_title`), so its usage is in no
//! session rollout. The fixture carries no hook timing; the timings
//! under Known gaps come from a logging hook that read the live rollout
//! at each hook.
//!
//! `tests/fixtures/codex_tui_auto_compact_0.163.0.jsonl` holds a second
//! TUI session's hook payloads. Captured: codex-cli 0.163.0, 2026-10-24,
//! the same setup and rewrites as above, plus
//! `model_auto_compact_token_limit = 1000` in `config.toml`, which every
//! request's fake usage exceeds. Two prompts each ran one shell command
//! without approval. The first compacted mid-turn, after its tool call.
//! The second compacted before its `UserPromptSubmit` and again
//! mid-turn. Each compaction fires `PreCompact` and `PostCompact`
//! (trigger `auto`), then `SessionStart` with source `compact` as the
//! next model request starts. `SessionEnd` fired when the daemon was
//! stopped. In a third TUI run (not kept), a manual `/compact` followed
//! by a prompt fired `SessionStart{compact}` only with that prompt,
//! before its `UserPromptSubmit`.
//!
//! `tests/fixtures/codex_exec_post_turn_compact_0.163.0.jsonl` holds one
//! `codex exec` run's hook payloads. Captured: codex-cli 0.163.0,
//! 2026-10-24, `codex exec --dangerously-bypass-hook-trust
//! --skip-git-repo-check <prompt> < /dev/null` (embedded mode, with no
//! approval or sandbox flag) in the same sealed setup, with
//! `model_post_turn_compact_threshold_percent = 1` and
//! `model_context_window = 100000`, paths rewritten as above. Its
//! payloads report `permission_mode: "bypassPermissions"`. One prompt
//! ran one shell command. Codex compacted after the turn's `Stop`, and
//! nothing followed `PostCompact` until `SessionEnd`. Whether the real
//! backend turns post-turn compaction on by default is unverified; with
//! only the token limit set, no run compacted after `Stop`.
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
//!     discovery's `pending-<pid>` row for each TUI never reconciles.
//!     Each session's `SessionStart` (startup, resume, clear or compact)
//!     with an id other than the row's ends the row and starts a fresh one
//!     under its id, so the row follows the session that started last. The
//!     first `SessionEnd` from any session removes the row, unless a
//!     `SessionStart` ended that session in the daemon's pid in the last
//!     10 s (atmd's ended-session marker). Checked with two TUIs on one
//!     daemon (codex-cli 0.163.0, sealed fake provider, 2026-10-26): B's
//!     `SessionStart{startup}` and then A's `SessionStart{compact}` after
//!     an auto compaction each moved the row, and stopping the daemon
//!     fired `SessionEnd{other}` for both sessions;
//!   - `TMUX_PANE` in the hook's environment is the daemon's, inherited
//!     from whichever TUI started it, so other sessions show that pane;
//!   - `/quit` detaches ("Any running work continues") and fires no
//!     `SessionEnd`; it arrives when the daemon stops;
//!   - one session's events overwrite another's state on the shared
//!     row. While session A waits on approval, session B's `PreCompact`
//!     turns the row from needs-input to Working/"Compacting" and B's
//!     `Stop` sets it idle, hiding A's approval either way. While A runs
//!     a tool, B's `PostCompact` sets the row idle until A's next event.
//!
//!   From code reading, not run: atm's kill, interrupt (`C-c`) and
//!   `atm send` act on the row's pane, so they reach the daemon's
//!   pane, which may be another session or, once its TUI has quit, a
//!   shell; and a row created from a hook event takes its project from
//!   `/proc/<daemon pid>/cwd`.
//!
//!   `codex --no-daemon` restores one process per session: each hook
//!   sees its own TUI's pid and pane, and `/quit` fires `SessionEnd`
//!   (checked with two TUIs). `--dangerously-bypass-hook-trust` also
//!   forces this embedded mode. Nothing in the hook's environment names
//!   the client TUI. Fix pending, a user decision. The TUI's
//!   `--no-daemon` flag is absent from codex-cli 0.155.0 and present in
//!   0.158.0 (static check of the npm binaries, not run); that the
//!   daemon is the default is checked on 0.163.0 only.
//! - **A failed compaction leaves the row "Compacting".** When the fake
//!   provider answered the summary request with a 400 (codex retried it
//!   6 times), a manual `/compact` fired `PreCompact` and nothing else
//!   (one TUI run, not kept). No hook ends it, so the row stays
//!   Working/"Compacting" until the next prompt.
//! - **A mid-turn auto compaction shows the row idle.** The turn goes on
//!   to its `Stop`, but `PostCompact` (mapped to `WorkingEnd`) and the
//!   `SessionStart{compact}` after it (atm-core's `SessionStart`) both
//!   set Idle. The row shows idle while codex works, until its next tool
//!   call or `Stop`. atmd's `test_e2e_codex_captured_auto_compact_replay`
//!   pins this. In a timed TUI run against the fake provider,
//!   `SessionStart{compact}` started about 25 ms after `PostCompact` and
//!   before the next model request, so mapping `PostCompact` to
//!   `WorkingEnd` brings the idle forward by about one hook's runtime.
//!
//!   Unverified, from code reasoning: the adapter maps each payload on
//!   its own, and a mid-turn `PostCompact{auto}` and one after `Stop`
//!   both carry their own turn's `turn_id`, so telling them apart needs
//!   state from outside the payload. Candidates: the row's status before
//!   `PreCompact`, restored when compaction ends (an atm-core change),
//!   or the rollout's `task_started`/`task_complete` records, which
//!   atmd already reads at each codex hook (their timing relative to
//!   `PostCompact` not checked). Claude likely has the same gap: Claude
//!   Code 2.1.310's compaction routine, which takes the trigger as a
//!   parameter, fires `SessionStart{compact}` right after `PostCompact`
//!   (static read of its bundle, not run).
//! - **Usage lags one request mid-turn.** A logging hook that read the
//!   live rollout at each hook saw no `token_count` at `PreToolUse`,
//!   `PermissionRequest` or `PostToolUse` of a turn's first request, and
//!   both of the turn's at `Stop` (every TUI and exec run). Codex writes
//!   a request's `token_count` after the tool output it led to, but the
//!   `token_usage_record` as the reply completes. Reading that record
//!   might remove the lag (it has no context window); not tried. Not
//!   checked on 0.146.1.
//! - No `info: null` `token_count` placeholders appeared: 0 of 40 across
//!   `codex exec` and TUI runs, against a fake provider that sends no
//!   rate-limit headers, so the real backend is unverified.
//!   [`read_token_usage`] still skips them.
//! - `SubagentStart` and `SubagentStop` have never been captured.
//! - **A denied approval leaves the row needing input.** Captured:
//!   codex-cli 0.163.0, 2026-10-30, a sealed TUI on its default shared
//!   daemon with `approval_policy = "on-request"`, a fake provider whose
//!   `exec_command` asked for `sandbox_permissions: "require_escalated"`,
//!   and `atm send` driving it (#116). `PermissionRequest` fired about
//!   1 s before the approval dialog drew, with "1. Yes, proceed" picked by
//!   Enter. Esc on the dialog ("You canceled the request") sent no hook,
//!   so the row kept reading needs input until the next prompt, and
//!   `atm send` refused until then. The first Enter after very fast typed
//!   input (tmux `send-keys -l`) stayed in the composer, probably as part
//!   of a paste burst; a bracketed paste then Enter submitted at once.
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
