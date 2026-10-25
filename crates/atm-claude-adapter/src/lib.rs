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
//! - [`event`] — `ClaudeEventType` enum (the 17 Claude hook event names
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
//!   teammate. Like every child event, they leave the lead's status
//!   alone. Until 2026-10-25 one that reached an idle lead turned it
//!   working until its next hook (read from the code; when they arrive
//!   was not captured).
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
//! `RawStatusLine`. Beside them, `claude_status_line_blank_model_2.1.310.json`
//! records a status-line quirk: Claude reports a whitespace model setting
//! (`--model " "`, `ANTHROPIC_MODEL=" "`) as `"model": {"id": "",
//! "display_name": "unknown model"}`; `atm-protocol`'s `parse.rs` docs
//! describe it. The fixtures move here when `RawStatusLine` does.
//!
//! `tests/fixtures/claude_subagent_run_2.1.267.jsonl` holds one Agent
//! run that finished inside the Agent call. Captured: Claude Code
//! 2.1.267, date and method not recorded; added in d433a4b on
//! 2026-10-03, whose message calls it a scrubbed capture of a real
//! subagent run.
//!
//! `tests/fixtures/claude_stop_failure_2.1.310.jsonl` holds three TUI
//! sessions whose one turn ended on an API error, five hooks each:
//! `SessionStart`, `UserPromptSubmit`, `StopFailure`, `Notification`
//! (`idle_prompt`), and a `SessionEnd` (reason `other`) from tearing the
//! capture down. Captured: Claude Code 2.1.310, 2026-10-25, by the
//! method above but left in auto mode (`permission_mode: "auto"`), with
//! the fake Messages API failing each turn: a 500
//! (`error: "server_error"`), a 429 (`rate_limit`), and a stream cut
//! after its first text delta (`unknown`). Paths were rewritten as
//! above. The fake API's `127.0.0.1` port in `last_assistant_message` is
//! verbatim.
//!
//! `tests/fixtures/claude_background_agent_dialog_2.1.310.jsonl` holds
//! two TUI sessions, 14 hooks and then 20, whose permission dialog stayed
//! open while background subagents started and stopped (see Background
//! agents and dialogs below). Captured: Claude Code 2.1.310, 2026-10-25,
//! by the method above in manual mode, with the fake Messages API holding
//! a subagent's replies for seconds to keep it running. Each ends with a
//! `SessionEnd` (reason `other`) from tearing the capture down. Paths
//! were rewritten as above.
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
//! ## API errors (captured 2026-10-25)
//!
//! A turn that ends on an API error sends `StopFailure` instead of
//! `Stop`, and ATM ends the turn on it. A status line follows about
//! 310 ms later and `Notification` (`idle_prompt`) 60 s later, as after
//! `Stop`. Sealed TUI runs on Claude Code 2.1.310 against a fake
//! Messages API, counting the turn's own requests:
//!
//! | Fake API reply | Requests | `StopFailure` after | `error` |
//! |---|---|---|---|
//! | 500 `api_error` | 11 | 178 s | `server_error` |
//! | 529 `overloaded_error` | 3 | 1.9 s | `server_error` |
//! | 400 `invalid_request_error` | 6, within 0.2 s | 0.35 s | `unknown` |
//! | 429 `rate_limit_error` | 11 | 173 s | `rate_limit` |
//! | Connection cut mid-stream | 2, then 1 without streaming | 3.7 s | `unknown` |
//! | SSE `error` event mid-stream | 1, then 1 without streaming | 2.1 s | `unknown` |
//!
//! A 500 followed by a good reply ended with `Stop`. While Claude retries
//! the row reads working, which matches the TUI's "Retrying in …" line.
//! The payload carries `error` and `last_assistant_message` (the error
//! text the TUI shows), and never had the schema's optional
//! `error_details`.
//!
//! - Unlike `Stop`, `StopFailure` lists no `background_tasks` or
//!   `session_crons` (neither its schema nor the captures has them). So
//!   after a failed turn the parent shows no "N running" summary, and
//!   children left working are not swept until a later quiet `Stop`.
//! - In `claude -p` (9 runs, each a 400), `StopFailure`'s hook started
//!   about 115 ms before `SessionEnd`'s, and no row was left after the
//!   process exited. Claude doesn't await the hook, so the order is
//!   observed, not guaranteed.
//! - Read from the binary, not run: the hook's `error` values also
//!   include `overloaded`, `authentication_failed`, `billing_error`,
//!   `max_output_tokens` and others. A subagent's API error sends no
//!   `StopFailure`. Claude also sends it for turns that end without an
//!   API error: a prompt too long, a tool call or reply still malformed
//!   or empty after a retry, and auto-compact thrashing.
//! - Not checked: a `StopFailure` after which Claude goes on working,
//!   such as a quota auto-resume after `rate_limit`. The row would read
//!   idle until the next hook.
//!
//! ## Background agents and dialogs (captured 2026-10-25)
//!
//! Background subagents start and stop on their own schedule, so their
//! `SubagentStart` and `SubagentStop` can reach the lead while it waits on
//! a permission dialog. The lead sends no hook of its own while its
//! dialog waits; a background agent's own hooks carry its `agent_id` and
//! go to its row. So child events leave the lead's status alone: until
//! 2026-10-25 they set it working, and the row lost its needs input until
//! the user answered. Sealed TUI runs on Claude Code 2.1.310 in manual
//! mode, with `atm list` polled every 200 ms:
//!
//! - A background agent that finished 15 s into the dialog sent its
//!   `SubagentStop` then. The task notification's `UserPromptSubmit`
//!   came only after the answer, in the same turn as the approved call.
//! - Subagents get the `Agent` tool. A background agent that spawned a
//!   nested agent during the dialog sent a `SubagentStart` then; when the
//!   nested agent finished, Claude restarted its spawner with the task
//!   notification, a new `SubagentStart` under the spawner's old id.
//! - The lead's own calls can't do it: given `[gated Bash, Agent]` in one
//!   message, Claude started the agent 71 ms after the dialog's answer.
//! - Stopping a background agent from the task list (`x`) sent no
//!   `SubagentStop`, only the task notification's `UserPromptSubmit` and
//!   a quiet `Stop`. That `Stop` swept the child because it still read
//!   working; the sweep skips a child waiting on input, so a child stopped
//!   while it waits keeps its row (read from the code, not run; it
//!   predates 2026-10-25). A natural `SubagentStop` at
//!   an idle lead was followed by that `UserPromptSubmit` 53 ms later, so
//!   no captured case needed a child event to mark the lead working.
//!
//! ## Known gaps (Claude Code 2.1.310)
//!
//! - **A nested subagent's row sits directly under the lead.** Its
//!   `SubagentStart` carries only `agent_id` and `agent_type`, nothing
//!   naming the subagent that spawned it, so atmd nests it under the
//!   lead, beside its spawner. A fix can't just nest it under the
//!   spawner: the spawner's `SubagentStop` came before the nested
//!   agent's, and removing a session removes its children, so the running
//!   nested agent's row would go with it.
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
//!   8 to 30 ms later in the captures, still carrying the ended session's
//!   id. atmd drops status lines with that id from the same pid for 10 s
//!   after the `SessionEnd`. Before that, the late line re-created the
//!   ended session's row. `claude --resume <id>` runs in a new process,
//!   so its status lines count at once. Left open: an in-process `/resume`
//!   back to a session that ended in that process under 10 s earlier
//!   would lose its status lines until the 10 s pass (read from the code,
//!   unverified).
//! - **A user's interrupt fires no hook.** In sealed TUI runs, pressing
//!   Esc while Claude streamed a reply, and denying a permission dialog
//!   in manual mode (Esc or "No"), sent no hook at all: no `Stop`, no
//!   `PostToolUseFailure`, no `PermissionDenied`, and no `idle_prompt`
//!   within 75 s. Only a status line came, within about 0.3 s, and status
//!   lines leave a row's status alone (see above). So the row stays
//!   working, or needs input after a denied dialog, until the next
//!   prompt. A fix needs a signal Claude doesn't send today. An Esc
//!   while a tool runs was not tried (`PostToolUseFailure` has an
//!   `is_interrupt` field).
//! - **ATM registers 17 of the 33 hook events that 2.1.310 declares**
//!   (read from the event list in its binary). The others are
//!   `PostToolBatch`, `UserPromptExpansion`, `PostCompact`,
//!   `PreModelSwitch`, `PostModelSwitch`, `PermissionDenied`,
//!   `Elicitation`, `ElicitationResult`, `ConfigChange`,
//!   `WorktreeCreate`, `WorktreeRemove`, `InstructionsLoaded`,
//!   `CwdChanged`, `FileChanged`, `DirectoryAdded` and `MessageDisplay`.
//!   Per Claude's changelog (not run), `StopFailure` arrived in 2.1.78
//!   and `TaskCreated` in 2.1.84, and before 2.1.101 an unknown hook
//!   name made Claude ignore the whole settings.json. So ATM's hooks need
//!   Claude Code 2.1.84 or later, as they did before `StopFailure`.
//!   Those that looked as if they could change a row's state were
//!   captured on 2026-10-25 and left unregistered:
//!   - `PermissionDenied` fires only when the auto-mode classifier
//!     denies a tool call, not when the user denies a dialog. The run
//!     sent `PreToolUse`, `PermissionDenied`, no `PostToolUse*`, then
//!     the model's next reply and its `Stop`. The row stays working and
//!     shows the denied tool until the next hook.
//!   - A session-only model switch (the `meta+p` picker, then `s`)
//!     fires `PreModelSwitch` and `PostModelSwitch` (source `picker`).
//!     The status line about 300 ms later carries the new model, which
//!     updates the row.
//!   - After `/compact`, `PostCompact` (trigger `manual`) comes 23 ms
//!     after `SessionStart` (source `compact`) has set the row idle, so
//!     it would add nothing. It also carries `compact_summary`, the
//!     conversation summary, which atm-hook would send to atmd.
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
