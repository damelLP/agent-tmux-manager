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
//! - [`event`] — `ClaudeEventType` enum (the 16 Claude hook event names
//!   ATM registers)
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
//!
//! ## Captured payloads
//!
//! `tests/fixtures/claude_tui_session_2.1.310.jsonl` holds the hook
//! payloads of one TUI session, one per line, as Claude wrote them to the
//! hook's stdin. Captured: Claude Code 2.1.310, 2026-10-24, Claude's TUI
//! in a private tmux server, inside a network namespace with only
//! loopback, with an isolated HOME and CLAUDE_CONFIG_DIR, answered by a
//! local fake Messages API (`ANTHROPIC_BASE_URL` and a fake
//! `ANTHROPIC_API_KEY`; no model call). `atm setup` wrote the hooks and
//! the statusLine, and a wrapper logged each stdin before the real
//! `atm-hook` ran. The project and config paths were rewritten to
//! `/home/user/project` and `/home/user/.claude`, and Claude's
//! `/tmp/claude-<uid>` task directory to `/tmp/claude-user`. Two traps
//! for the next capture:
//!
//! - Auto mode is the default permission mode in 2.1.310, so the run
//!   switched to manual mode (payload `permission_mode: "default"`) to
//!   get permission dialogs.
//! - Claude writes background-agent output under the OS temp directory,
//!   outside an isolated HOME. Set `TMPDIR` to keep it in the capture.
//!
//! The session ran a Bash call approved in the dialog, a failing Bash
//! call (approved), and an Agent call whose subagent ran one Bash call,
//! then `/compact`, `/clear` and `/exit`. A second process resumed the
//! first session with `--resume` and exited. Both processes were launched
//! with `--model claude-opus-5-5`, and `SessionStart` carried `model` for
//! sources `startup` and `compact` only (unverified without `--model`).
//! Claude sends no `pid` or `tmux_pane`; `atm-hook` adds them.
//!
//! For the events both captures have, every field the adapter reads is
//! unchanged from 2.1.267, and this capture has only extra fields:
//! `effort`, and on `SessionStart` `model`, `prompt_id` and, for
//! `resume`, `seconds_since_last_response`, `context_tokens`,
//! `prompt_cache_likely_expired` and `estimated_cache_write_usd`. The
//! 2.1.267 capture's method is not recorded, so some of these may come
//! from the method rather than the version. `PostToolUseFailure`,
//! `PermissionRequest`, `Notification` and `PreCompact` are captured
//! here for the first time, so their fields have no earlier capture to
//! compare with.
//!
//! The same run's 14 status lines are in `atm-protocol`, at
//! `tests/fixtures/claude_status_line_2.1.310.jsonl` next to
//! `RawStatusLine`. The fixture moves here when `RawStatusLine` does.
//!
//! `tests/fixtures/claude_subagent_run_2.1.267.jsonl` holds one Agent
//! run that finished inside the Agent call. Captured: Claude Code
//! 2.1.267, date and method not recorded; added in d433a4b on
//! 2026-10-03, whose message calls it a scrubbed capture of a real
//! subagent run.
//!
//! The JSON payloads in the `wire.rs` tests are hand-written (see
//! `Synthetic:` there). Tests that build `RawHookEvent` structs check
//! mapping logic and claim no capture. Claude-shaped JSON in other
//! crates' tests is hand-written too unless marked `Captured:`: in
//! atm-protocol (`message.rs`, `parse.rs`, `tests/proptest_roundtrip.rs`),
//! in atmd (the registry actor's and handle's unit tests,
//! `tests/registry_integration.rs`, `tests/robustness_tests.rs`,
//! `tests/server_integration.rs`) and in the root
//! `tests/e2e_with_tmux.rs`.
//!
//! ## Known gaps (Claude Code 2.1.310)
//!
//! - **`claude --bare` runs the statusLine but no hooks, so ATM can't
//!   see its turns.** A sealed TUI run with `--bare` sent status lines at
//!   startup, after a permission-mode change and after a turn, and no
//!   hook events. Status lines leave a row's status alone, because Claude
//!   also sends one about 300 ms after every `Stop` (317, 307 and 307 ms
//!   in the capture), and after `/compact`, `/clear` and a
//!   permission-mode change at the idle prompt. So a `--bare` row stays
//!   idle while Claude works. It was wrong when every status line set
//!   Working too: the row then stayed working at the idle prompt. In the
//!   same kind of run, `disableAllHooks: true`, a managed
//!   `allowManagedHooksOnly: true` and `--safe-mode` each stopped both
//!   the hooks and the statusLine, so ATM hears nothing from those
//!   sessions.
//! - **The row reads idle for one model response after an auto-compact
//!   or a blocked `Stop`.** Both were checked in sealed TUI runs with
//!   replies streamed over 8 s:
//!   - A user's own `Stop` hook that returns `decision: block` makes
//!     Claude continue. ATM's `Stop` has already set Idle, and neither a
//!     hook nor a status line marks the continuation. The next hook is
//!     its first `PreToolUse`, or a second `Stop` with
//!     `stop_hook_active: true`.
//!   - An auto-compact at the start of a turn fires `UserPromptSubmit`,
//!     `PreCompact` (trigger `auto`), a `SubagentStop`, then
//!     `SessionStart` (source `compact`), which sets Idle while Claude
//!     goes on to answer. `SessionStart` has the same fields after a
//!     manual `/compact`, where Idle is right; only `PreCompact` carries
//!     the trigger.
//!
//!   In both cases the row stays idle until the response's first
//!   `PreToolUse`, or its `Stop`. `atm list` in those runs:
//!
//!   | Build | 3 s into the response | After the turn |
//!   |---|---|---|
//!   | Status lines set Working (before #81) | `working` | `working` (~60 s) |
//!   | Status lines leave status alone | `idle` | `idle` |
//!
//!   The fix needs core lifecycle state; see the #79 follow-up.
//! - **The Agent tool runs subagents in the background by default.**
//!   `PostToolUse` for `Agent` follows `SubagentStart` at once, with
//!   `tool_response` `{"isAsync": true, "status": "async_launched",
//!   "agentId": ..}`. The lead's `Stop` then fires while the subagent
//!   runs, listing it in `background_tasks` with `"status": "running"`,
//!   so the parent's busy `Stop` keeps the child. After `SubagentStop`,
//!   Claude submits a `UserPromptSubmit` whose `prompt` is a
//!   `<task-notification>` block, then a quiet `Stop`. A row that already
//!   has a first prompt keeps it. A row with none, such as one found by
//!   discovery after atmd restarts during a background run, would take
//!   that block as its first prompt (read from the code, unverified).
//!   `SubagentStop` lists its own subagent as still running; only `Stop`
//!   is read for background work.
//! - **`/compact` fires `PreCompact` (trigger `manual`), a
//!   `SubagentStop` with `agent_type: ""` and no `SubagentStart`, then
//!   `SessionStart` (source `compact`), and no `Stop`.** The
//!   `SubagentStop` is the compaction's own agent. It may be one source
//!   of the unmatched `SubagentStop`s in the agent-teams notes above
//!   (unverified for teams).
//! - **A status line also follows `SessionEnd`** (`/clear` and `/exit`),
//!   still carrying the ended session's id, and atmd re-creates the row
//!   from it. After `/clear`, the new session's first status line
//!   renames that row to the new id. After `/exit`, atmd's dead-process
//!   cleanup removes it about 1 s later, logging "process died without
//!   a session-end signal" although one arrived.
//! - **ATM registers 16 of the 33 hook events that 2.1.310 declares**
//!   (read from the event list in its binary, not run). The others are
//!   `PostToolBatch`, `UserPromptExpansion`, `StopFailure`,
//!   `PostCompact`, `PreModelSwitch`, `PostModelSwitch`,
//!   `PermissionDenied`, `Elicitation`, `ElicitationResult`,
//!   `ConfigChange`, `WorktreeCreate`, `WorktreeRemove`,
//!   `InstructionsLoaded`, `CwdChanged`, `FileChanged`, `DirectoryAdded`
//!   and `MessageDisplay`. Those that could change a row's state:
//!   `StopFailure` (a turn that ends in an API error fires no `Stop`, so
//!   the row may stay Working; unverified), `PostCompact`,
//!   `PermissionDenied`, and the model-switch pair.
//! - **`PermissionRequest` comes about 6 s before `Notification`
//!   (`permission_prompt`) for the same dialog.** Both map to needs
//!   input. The `Notification` carries no tool, so the row's label
//!   changes from `Bash: <command>` to `Permission` when it lands
//!   (`atm list` showed `Permission` after it; the 6 s before were not
//!   checked). `PermissionRequest` has no `tool_use_id`.

pub mod event;
pub mod translate;
pub mod wire;

pub use event::ClaudeEventType;
pub use wire::RawHookEvent;
