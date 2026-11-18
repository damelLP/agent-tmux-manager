//! pi adapter for ATM.
//!
//! All pi-specific knowledge — the event vocabulary, the wire payload
//! shape, and the translation into vendor-neutral
//! `atm_core::LifecycleEvent` — lives in this crate. Symmetric with
//! `atm_claude_adapter`. `atm-core` holds pi's built-in harness metadata
//! and `atm-protocol` its `PiEvent` wire envelope, two exceptions
//! documented in the Architecture section of the repository's AGENTS.md.
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
//! The complete translation (`RawPiEvent::to_lifecycle_events`) also forwards
//! the envelope's current model after the original event, so startup/resume
//! needs no `model_select`. Derived metadata is not message activity.
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
//! and back to Idle when final closure measures idle=true, otherwise
//! `WorkingStart`. The adapter still maps a `tool_call`
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
//! `/tmp/xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx.jsonl`, not
//! kept), pi-spike tracer in an interactive session, spike
//! `agent-tmux-manager-9dn`; added in 7a20d86 on 2026-05-10.
//!
//! Tests that build `PiPayload` structs check mapping logic and claim
//! no capture. `Synthetic:` marks the tests and examples whose shapes
//! or values no capture produced.
//!
//! ## Conversation bodies and busy-row limits
//!
//! pi-atm sends empty payloads for `context`, `turn_end` and `agent_settled`.
//! Context usage/model come from the envelope. Legacy `agent_end` frames,
//! with or without conversation bodies, still become `WorkingEnd`.
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed SDK session with a scripted local
//! reply (no model call), events and outgoing Unix-socket frames measured.
//! Seeded conversation text and the scripted reply were each 1,100,000
//! characters: pi's raw context, turn-end and agent-end events each exceeded
//! 1 MiB. Before thinning, context and agent-end wire frames exceeded atmd's
//! 1 MiB line limit too; turn-end was already trimmed. The envelope carries
//! the usage snapshot without those bodies.
//!
//! Synthetic: the same day's private-atmd probe fed the original extension
//! an oversized agent-end callback. The daemon closed that connection and
//! kept Working; an end sent during reconnect backoff was lost too. A small
//! end after reconnect restored Idle. This is not attribution of a live
//! stale-row incident, nor a general size bound on other event bodies.
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed SDK session, an input handler
//! consumed an interactive input, original pi-atm and a private real atmd.
//! Pi emitted session-start and input, but no agent-start or agent-end:
//! pi was idle while atmd showed Working. No model was called. Payload
//! thinning cannot fix an event that never fires. This handled-input gap
//! remains; eligible idle reconnect recovery below is separate, not a
//! heartbeat. Neither probe verified the reported live cause.
//!
//! ## Final settlement (pi 1.5.2+ forwarder)
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY, free in-process
//! static provider, callback recorder and socket proxy to a private atmd.
//! `tests/fixtures/pi_turn_events_1.5.2.jsonl` projects real callbacks, with
//! measured ctx idle/pending alongside (not inside) payloads. Session ids
//! and private paths are anonymized. It covers continuation, skills, Esc,
//! manual and automatic compaction, overflow recovery, new/reload, and the
//! then-unfixed handled-input, standalone-dialog and idle-reconnect gaps.
//! Additional `late-failure/queued-failure` projection. Captured: pi 1.5.2,
//! 2026-11-16, sealed interactive PTY/private atmd, an owned
//! loopback fake HTTP provider held open and an earlier async failure
//! handler held on a socket acknowledgment (no model call). The failure's
//! `stage: after-earlier-async-handler` measures ctx when that handler
//! resumes, before pi-atm runs, not at initial notification dispatch.
//! `pi_turn_frames_1.5.2.jsonl` records the changed forwarder's actual frames
//! from the original rig, with anonymous ids/PIDs and case/run labels.
//!
//! At `agent_end`, pi is still busy and may continue; only `agent_settled`
//! is final (including aborts). The modern extension requires pi 1.5.2+ and
//! forwards settled instead of end; the adapter retains end for old
//! forwarders. Upgrade the daemon before the extension. Manual compaction
//! emits success/failure but no settled, so those outcomes map to Idle only
//! for reason=manual. Automatic outcomes await settled, even when willRetry
//! is false. Manual success notifies before pi clears its compacting flag;
//! later async completion handlers can still run. Manual failure clears
//! that flag before notifying: queued work can start during an earlier
//! async failure handler. The forwarder suppresses that stale failure when
//! current ctx.isIdle() is false, preserving the newer running turn.
//! Only compaction reason is
//! forwarded, never preparation, branch history, summaries or error text.
//!
//! ## Historical input idle snapshots
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY, free in-process
//! static provider, snapshot-sending pi-atm and private-atmd socket proxy.
//! The `input2-*` frames in `pi_turn_frames_1.5.2.jsonl` retain those earlier
//! input snapshots for regression tests and installed-forwarder compatibility.
//! The 0.4.1 forwarder no longer adds `payload.idle`; the adapter ignores it
//! on historical frames. Interactive input remains one raw `PromptSubmit`.
//! A derived input Idle double-counted the start-line turn and could loop
//! automatic handover. That correction is reverted for 0.4.1; atomic
//! primary-plus-derived publication is deferred to stage 2. A later input
//! consumer can therefore still leave the row Working without an agent run.
//!
//! ## Idle reconnect registration (pi 1.5.2)
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed interactive CLI/private tmux and
//! real atmd recording proxy; `tests/fixtures/pi_reconnect_1.5.2.jsonl`.
//! Startup, daemon restart, /new and /reload emitted idle snapshots with
//! zero input, agent-start or provider calls. Discovery's pending row could
//! not bind; recovery upgraded its real identity/model/Idle, and existing
//! adoption policy then bound it without fabricated start proof. IDs/PIDs
//! and pane attribution in the fixture are anonymized.
//!
//! The same fixture's held-input trace was captured with a sealed real SDK
//! session/private receiver. The actual socket connect callback and a later
//! input handler were held: ctx reported idle=true, pending=false before
//! agent_start. A local request-preparation fault ended the subsequent real
//! run before any provider call. The input veto suppresses the reconnect
//! snapshot in that window; settlement permits the next reconnect snapshot.
//!
//! `atm_idle` maps only real nonpending identity + nonzero pid to Idle and
//! optional model metadata, never start/input/end/usage or an exact receipt.
//! One snapshot follows each handshake and fully drained bounded FIFO, using
//! current live context and vetoing busy/pending/native UI/wrapped selects.
//! The factory owns sockets and unref'ed retry/connect timers, and shutdown
//! disposes them even when the imported module or UI object is reused.
//! A failed predicate is consumed, not polled. Interactive input latches a
//! veto until genuine agent_start/agent_settled/session_start; input consumed
//! or rejected without a run therefore withholds recovery until that boundary.
//! No busy/attention/usage replay, periodic heartbeat or stronger PID policy.
//!
//! ## Select-dialog closure (pi 1.5.2)
//!
//! Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY/local slash command,
//! changed pi-atm and private real atmd recording proxy;
//! `tests/fixtures/pi_dialog_1.5.2.jsonl`. Enter selected Alpha and Esc
//! cancelled; both closed with measured idle=true/pending=false and the
//! row returned NeedsInput -> Idle, with zero provider/input/agent-start calls.
//! Session ids/PIDs are anonymized; no conversation or screens are retained.
//!
//! Overlapping wrapped selects emit one outer open/final-resolved pair.
//! Final completion (including error) samples the live owner's ctx.isIdle,
//! never the opening state. Only boolean measurements are sent; payload.idle
//! is optional and strictly boolean/null on receipt. Only resolved dialogs
//! consume it: true -> Idle; false/missing/null -> legacy WorkingStart.
//! Interactive input's conservative latch also withholds this measurement
//! until genuine agent_start/agent_settled/session_start; consumed input can
//! therefore still leave Working until that boundary. Missing getters omit
//! the measurement; stale throwing ctx or disposed owners suppress reporting
//! without changing the original select result/error. No prompt, start, usage
//! or WorkingEnd is manufactured. Historical input idle correction stays
//! reverted; its fixtures still map to a single PromptSubmit.
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
//! takes the last reply on the current branch that wasn't aborted, as
//! those versions' footers do, counted as pi's `calculateContextTokens`
//! counts it. Checked: pi 0.73.1, 0.50.0 and 0.48.0, two prompts and three
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
//!   The complete translation uses pi-atm's envelope snapshot instead;
//!   older pi-atm without that snapshot still needs a model switch.
//! - The context window comes from that snapshot, on `context` events.
//!   pi-atm before the snapshot sends none, so context percentages use
//!   the unknown model's 200K default until the user re-runs `atm setup`.
//! - After a model switch, the row pairs the new model with the old
//!   model's window until the next `context` event, which comes with the
//!   next prompt. `model_select` carries the new window, but
//!   `ProviderModelChange` has no field for it, and adding one is an
//!   atm-core change. Checked: one prompt on fake-a, then Ctrl+P, and
//!   `atm list` showed fake-b at `0.8% (1.0K/128K)`.
//! - pi-atm before the usage snapshot (097f4f4 and earlier) sends no
//!   totals, until the user re-runs `atm setup`. The adapter then takes
//!   the latest reply in each `context` event, so the row shows one
//!   request's cost rather than the total, a reply behind. Checked: that
//!   pi-atm, with the snapshot-aware atmd, showed $0.0033, then
//!   $0.003675, where the footer showed `$0.007` and `$0.011`.
//! - After `/resume`, `/fork` or `/reload`, the new row shows $0 until the
//!   next prompt's first `context` frame, while pi's footer shows the
//!   session file's total at once. Unlike the model, usage is not
//!   snapshotted on `session_start`, so even the complete translation
//!   cannot restore the cost from that frame.
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
//!   (`handle_remove_by_pid`, atmd `registry/actor/remove.rs`), and the TUI
//!   keys rows by `SessionId` (atm-tui `app.rs:55`), so the two collapse
//!   into one. Claude has the same case with `claude --resume`. Queued as
//!   "one session id held by two pids". Older pi-atm sent no
//!   `session_id` (it read `currentSessionId`, which no pi release has),
//!   so each pid got its own `pending-<pid>` and the case never arose.
//! - Discovery sees pi by its bare name. pi sets `process.title = "pi"`
//!   (found in 0.50.0, 0.65.0, 0.70.0, 0.72.1, 0.73.1 and 1.5.0, not in
//!   0.10.0), so `/proc` shows cmdline `pi` with NUL padding, comm `pi`
//!   and exe `node`. Captured: pi 1.5.0, 2026-11-15, `/proc` read of
//!   running sessions. pi's registry entry therefore matches a bare `pi`
//!   as argv0. A pi already running when atmd starts gets a
//!   `pending-<pid>` row, without its model, cost or session id until
//!   pi-atm's next event or eligible idle reconnect snapshot. Before that
//!   match existed, such a pi had no row until an event. A real pi across a
//!   private-daemon restart now verifies idle identity/model recovery as
//!   documented above; cost/usage replay remains out of scope.
//! - `/new`, `/resume`, `/fork` and `/reload` replace the session in the
//!   same process. pi (0.65.0 and later) loads pi-atm afresh, so
//!   `session_shutdown` comes from the old instance and `session_start`
//!   from the new one on a new connection. atmd ends the old row and
//!   starts a new one. That includes `/reload`, which keeps the id, so
//!   the row's cost resets (the original check went from fake-b at
//!   $0.0033 to Unknown at $0; complete translation now restores the
//!   model from the snapshot). With older forwarders, the old instance's
//!   connection stays open until atmd closes it after 300 s idle. Over rpc,
//!   `new_session` and `clone` fired `session_start` twice; the TUI's `/new`
//!   fired it once.
//! - atmd renames a row from `pending-<pid>` to a real id on any event.
//!   A `session_start` with another real id ends the session the pid's row
//!   held, as its `session_shutdown` would, and starts a fresh row. So if
//!   the old instance's `session_shutdown` is lost (pi-atm drops sends
//!   during its 1 s reconnect back-off), the new session still gets its
//!   own row. And if the shutdown arrives after the new `session_start`
//!   (both instances connect afresh when atmd has closed the idle
//!   connection), atmd keeps the new row. An in-memory fork's shutdown
//!   carries the fork's new id (see pi-atm), which no `session_start`
//!   ended in that pid, so atmd still ends that row by pid; if the fork's
//!   `session_start` came first, that shutdown removes the fork's row
//!   until its next event. Checked with pi 0.73.1 over rpc (`--offline
//!   --no-session`, a fake provider, 2026-10-26): `new_session` and
//!   `clone` each sent the old instance's shutdown ~3 ms before the new
//!   one's start, the fork's shutdown carried the new id, and with the
//!   shutdown held 0.5 s by a socket proxy, `new_session` kept the new
//!   row while `clone` lost it, as described.
//! - pi 0.50.0 to 0.64.0 switch sessions in place: one pi-atm instance
//!   throughout, and only `session_switch`, which pi-atm doesn't forward.
//!   pi-atm reads the id on every event, so its frames follow the switch,
//!   but atmd keeps the row's first id: with no `session_start`, nothing
//!   replaces a real id. Checked with pi 0.64.0 over rpc: after
//!   `new_session`, frames carried the new id and `atm list` still showed
//!   the first.

pub mod event;
pub mod screen;
pub mod translate;
pub mod wire;

pub use event::PiEventType;
pub use wire::RawPiEvent;
