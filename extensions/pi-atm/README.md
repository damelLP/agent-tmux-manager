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

If `atmd` is not running (socket missing), the extension is a no-op —
pi keeps working unaffected.

## Forwarded events

Only events that drive `LifecycleEvent` translation are forwarded
(keeping wire traffic low):

| pi event                | Reason forwarded                                      |
|-------------------------|-------------------------------------------------------|
| `session_start`         | → `LifecycleEvent::SessionStart`                      |
| `session_shutdown`      | → `LifecycleEvent::SessionEnd`                        |
| `agent_start`           | → `LifecycleEvent::WorkingStart`                      |
| `agent_end`             | → `LifecycleEvent::WorkingEnd`                        |
| `tool_call`             | → `LifecycleEvent::ToolCallStart` (or `NeedsInput` when amplike's permission gate triggers via `ctx.ui.select`) |
| `tool_execution_end`    | → `LifecycleEvent::ToolCallEnd`                       |
| `model_select`          | → `LifecycleEvent::ProviderModelChange` (`model` and `previousModel` cut to `{id, provider, contextWindow}`) |
| `input` (interactive)   | → `LifecycleEvent::PromptSubmit`                      |
| `session_before_compact`| → `LifecycleEvent::ContextCompactStart`               |
| `context`               | → `LifecycleEvent::ContextUpdate {tokens, cost, context window}`, before each request |
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

High-frequency events (`message_update`, `before_provider_request`,
`turn_start`) and duplicates of forwarded events
(`tool_execution_start`, `tool_result`) are **not** subscribed — the
daemon-side adapter would suppress them anyway.

## Permission-gate integration

Pi has no dedicated permission-prompt event. Permission gating
happens *inside* an extension's `tool_call` handler via
`ctx.ui.select(...)`. pi-atm patches `ctx.ui.select` on the runner's
shared `uiContext` — so any extension (notably pi-amplike) that opens
a dialog automatically tells atmd "needs input now" before awaiting
the user, and "back to working" after. amplike doesn't need to know
about atm; atm doesn't need to know about amplike.

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
