# pi-atm

pi extension that forwards lifecycle events to the ATM daemon (`atmd`)
so pi sessions show up in the ATM TUI alongside Claude Code sessions.

This is the **runtime** counterpart to the Rust-side `atm-pi-adapter`
crate. The adapter knows how to translate pi events into vendor-neutral
`LifecycleEvent`; this extension is what subscribes to pi's event API
at runtime and feeds those events through to the daemon. It also
patches `ctx.ui.select` so any extension that opens a permission
dialog (notably pi-amplike) gets atm visibility for free — the TUI
flips to `needs input` while a dialog is open.

Tracks beads `agent-tmux-manager-6dx`.

## How it fits

```
                  ┌─────────────────────────────────────┐
                  │  pi (real, running)                  │
                  │  ┌────────────────────────────────┐ │
                  │  │  pi-atm (this extension)       │ │
                  │  └─────────────┬──────────────────┘ │
                  └────────────────┼─────────────────────┘
                                   │ pi event JSON
                              Unix socket /tmp/atm.sock
                                   │ MessageType::PiEvent
                                   ▼
                       ┌──────────────────────┐
                       │  atmd                │
                       │   ↓                  │
                       │  RawPiEvent          │
                       │   ↓ (atm-pi-adapter) │
                       │  LifecycleEvent      │
                       │   ↓                  │
                       │  Session registry    │
                       │   ↓                  │
                       │  TUI broadcast       │
                       └──────────────────────┘
                                   │
                                   ▼
                              atm TUI
```

## Install

Easiest: let `atm setup` do it for you. It auto-detects pi by
locating `~/.pi/agent/`, then writes the embedded extension to
`~/.pi/agent/packages/pi-atm/` and adds `"packages/pi-atm"` to
`~/.pi/agent/settings.json`'s `packages` array — no `pi install`
invocation needed since pi resolves local-package entries
relative to its `agentDir` directly:

```sh
atm setup
```

Or manually for local development:

```sh
pi --extension /abs/path/to/agent-tmux-manager/extensions/pi-atm/extensions/pi-atm.ts
```

pi loads `.ts` files directly via `@mariozechner/jiti`, so no compile
step is needed.

Once published to npm:

```sh
pi install npm:pi-atm
```

For maintainers: this directory is the source. `atm setup` installs
its `package.json` and `extensions/` from a copy embedded in
`crates/atm/assets/pi-atm/`. Edit the files here, then copy them
there; `tests/setup_pi_package.rs` fails if the two drift apart.

## Configuration

| Env var       | Default          | Purpose                                   |
|---------------|------------------|-------------------------------------------|
| `ATM_SOCKET`  | `/tmp/atm.sock`  | Daemon socket path                        |
| `ATM_DEBUG`   | unset            | When `1`, append debug to `/tmp/pi-atm.log` |

If `atmd` is not running, pi keeps working unaffected. The extension
buffers up to 256 frames (dropping oldest) and retries every 1000ms without
blocking pi. A pending connection has a 10s guard; an established idle
connection has no inactivity timeout.

## Forwarded events

The runtime extension requires **pi 1.5.2 or later** for final settlement and
compaction outcomes. Update `atmd` before installing this forwarder. The
adapter still accepts `agent_end` from older installed forwarders; this
extension no longer sends that low-level event because recovery or queued
continuation may follow it.

Only events that drive `LifecycleEvent` translation are forwarded
(keeping wire traffic low):

| pi event                | Reason forwarded                                      |
|-------------------------|-------------------------------------------------------|
| `session_start`         | → `LifecycleEvent::SessionStart`                      |
| `atm_idle` (extension-generated) | → `LifecycleEvent::Idle`, real identity only after handshake and queued frames drain |
| `session_shutdown`      | → `LifecycleEvent::SessionEnd`                        |
| `agent_start`           | → `LifecycleEvent::WorkingStart`                      |
| `agent_settled`         | → `LifecycleEvent::WorkingEnd`, after automatic continuation/recovery finishes (payload cut to `{}`) |
| `tool_call`             | → `LifecycleEvent::ToolCallStart` (or `NeedsInput` when amplike's permission gate triggers via `ctx.ui.select`) |
| `tool_execution_end`    | → `LifecycleEvent::ToolCallEnd`                       |
| `model_select`          | → `LifecycleEvent::ProviderModelChange` (`model` and `previousModel` cut to `{id, provider, contextWindow}`) |
| `input` (interactive)   | → `LifecycleEvent::PromptSubmit`                      |
| `session_before_compact`| → `LifecycleEvent::ContextCompactStart` (payload cut to `{reason}`) |
| `session_compact`, `session_compact_failed` | → `LifecycleEvent::Idle` for `reason: "manual"` only (payload cut to `{reason}`) |
| `context`               | → `LifecycleEvent::ContextUpdate {tokens, cost, context window}`, before each request (payload cut to `{}`) |
| `turn_end`              | → `LifecycleEvent::ContextUpdate`, after each reply is saved (payload cut to `{}`) |

Each frame also carries `model`, pi's current model (`ctx.model`) cut to
`{id, provider, contextWindow}`. The `context` event has no window of its
own, so atmd takes it from there. pi-atm never forwards a whole Model: it
can carry `headers`, which hold auth on pi 0.62 and earlier.

`context` and `turn_end` frames also carry `usage`, the session's totals
as pi's footer sums them: `{cost, tokens, contextTokens}` over every
assistant message in the session file, with `contextTokens` the size
behind the footer's context percentage. pi prices each reply on its own,
so atm takes the session's cost from this snapshot, not from a message.

`context` and `turn_end` send `{}` instead of conversation messages,
replies and tool results, which can exceed atmd's 1 MiB line limit and
close the connection. `agent_settled` also sends `{}`: the adapter needs
only the event name, not its aborted flag. Compaction events retain only
`{reason}`, omitting preparation, history, summaries and error text.
The envelope keeps session identity, pid, pane and model, plus `usage`
on `context`/`turn_end`. Older extension frames remain supported.
This is not a general frame-size cap: other event bodies can still be large,
and delivery remains best-effort during outages. It does not add heartbeat
or periodic state replay.

Maintainers can run the socket-level frame tests without pi installed:
`node --disable-warning=ExperimentalWarning --test tests/pi_event_frames.mjs tests/pi_turn_lifecycle.mjs`
from the repository root (Node 24).

High-frequency events (`message_update`, `before_provider_request`,
`turn_start`) and duplicates of forwarded events
(`tool_execution_start`, `tool_result`) are **not** subscribed — the
daemon-side adapter would suppress them anyway.

Manual compaction has no agent settlement event: its success, failure or
cancellation notification ends the compaction. A manual failure is not
forwarded when current `ctx.isIdle()` is false: an earlier asynchronous
failure handler can yield while a queued prompt starts a new run. Idle
failures/cancellations and manual success are unchanged. Automatic compaction outcomes
never imply idle, even with `willRetry: false`; final `agent_settled` does.
The manual success notification precedes pi clearing its compaction flag,
so later asynchronous completion handlers can still be finishing.

In 0.4.1, interactive `input` reports one raw-text `PromptSubmit` for
receipts and turn accounting, without an idle correction. The pre-run idle
snapshot correction was reverted because it counted the start-line turn
twice and could loop automatic handover. Atomic primary-plus-derived
publication is deferred to stage 2.

An input consumed by a later extension handler can still leave the row
Working without an agent run. A consumer before pi-atm short-circuits it
without an input frame; busy input does not clear work. The same pending-input
veto applies to idle dialog resolution below.

### Idle identity after reconnect

After handshake and FIFO backpressure clear, each connection gets at most
one `atm_idle` snapshot, using the current live session id/model, pid and
pane. It requires measured idle, no pending messages and no native UI or
wrapped select dialog. Missing/stale context fails closed. The snapshot
contains no usage, prompt, receipt, turn end or fabricated session start;
it upgrades discovered pending identity but does not supply launch proof.
A vetoed snapshot is consumed, not retried as a heartbeat.

Every interactive input sets a conservative veto. Only genuine
`agent_start`, `agent_settled` or `session_start` clears it: a later input
handler can still be running while pi reports idle. **Limitation:** input
consumed or rejected without a run withholds idle recovery until the next
genuine run/session boundary. Timers and idle=true never clear that veto.
Shutdown disposes the instance's sockets/queue/timers; late callbacks and
old select completions cannot reconnect or emit into the next session.

## Permission-gate integration

Pi has no dedicated permission-prompt event. Permission gating
happens *inside* an extension's `tool_call` handler via
`ctx.ui.select(...)`. pi-atm patches `ctx.ui.select` on the runner's
shared `uiContext` — so any extension (notably pi-amplike) that opens
a dialog automatically tells atmd "needs input now" before awaiting
the user. Overlapping selects share one open/final-resolved span, so an
inner completion cannot clear attention while a sibling is still open.
After the final completion (selection, Esc or error), the extension measures
the live `ctx.isIdle()`: true returns the row to Idle; false keeps Working.
Missing measurements/older frames keep the legacy Working fallback; an
invalidated context suppresses late reporting without changing the result/error.
Every interactive input vetoes that measurement until the next genuine
`agent_start`, `agent_settled` or `session_start`, even if pi reports idle.
An input consumed without a run therefore also withholds dialog idle recovery.
Native UI notifications remain local; no prompt, receipt or turn is fabricated.
amplike doesn't need to know about atm; atm doesn't need to know about amplike.

## Live test

1. Make sure atmd is running: `pgrep -af atmd` or `atmd start -d`.
2. In one terminal launch atm: `atm`.
3. In another terminal start pi with this extension:
   ```sh
   pi --extension /abs/path/to/agent-tmux-manager/extensions/pi-atm/extensions/pi-atm.ts
   ```
4. Ask pi to do something. Watch the new session appear in the atm TUI
   with `[pi]` badge and status flipping between `working` and `idle`.
5. Quit pi. The session disappears from the TUI.
