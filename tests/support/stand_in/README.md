# Shared stand-in harness (test only)

Python stdlib fixture for root integration tests. Never invokes a model CLI or
executes submitted text. Linux only (procfs process identity).

## Prerequisites and running

Required, never silently skipped: `python3`, `tmux` (including 3.6), `jq`, real
`socat`, `timeout`, and `/bin/bash`. Tools resolve from
`/usr/local/bin:/usr/bin:/bin`; no pip/npm packages. Missing prerequisites fail
explicitly. CI installs socat; repository hook scripts remain unmodified.

Run `cargo test --test stand_in_harness -- --nocapture` under the project's sealed
test environment, using Cargo's built binaries, not installed `atm`/`atmd`.
Python unittest runs from that Cargo target. Full workspace runs still require
the outer seal for older tests; this rig seals each subprocess independently.

## Consumer API (U7, U8, U9b)

Include `#[path = "support/stand_in.rs"] mod stand_in;` in a root integration test.
Use synchronous `#[test]` functions: protocol queries use a bounded internal Tokio
runtime, so don't nest them in an async runtime.

```rust,ignore
use atm_core::SessionStatus;
use stand_in::{StandInConfig, StandInMode, StandInRig};

let mut rig = StandInRig::start(); // real request round trip, not socket existence
let mut config = StandInConfig::new(StandInMode::Claude);
config.work_ms = 1200; // observation window, not a real-provider latency claim
config.clear_gap_ms = 1200;
let pane = rig.launch(&config); // current-process terminal-acknowledged first frame
let old = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
assert!(rig.atm(&["send", &pane.pane_id, "one\ntwo"]).status.success());
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
rig.wait_ready(&pane, Some(old.id.as_str()));
rig.paste(&pane, "/clear");
rig.wait_for_absent(&old.id);
let new = rig.wait_for_row(&pane, |r| r.id != old.id);
rig.wait_ready(&pane, Some(new.id.as_str()));
rig.close();
```

- `launch(config)` starts a named Python process in a new private window and
  retains a shell after exit. `StandInPane` exposes `pane_id`, `pid`, `launcher`,
  and `log`; actions reject handles from another rig.
- `launch` returns after the first terminal-acknowledged frame, not necessarily
  idle. `wait_ready(pane, optional_id)` waits on the private event stream for an
  idle frame at least as recent as a control barrier. Bracketed paste is enabled
  first; no `#{bracket_paste_flag}` or journal/screen polling is required. This
  is a rig milestone, **not** proof of empty input or readiness for delivery.
- `event_cursor(pane)` and `wait_event(pane, after, predicate)` replay and await
  sequenced runtime events with a 30-second outer hang guard. The listener is
  bound before launch. Every record has `pid`, procfs `start_time`, `pane`, and
  `event_sequence`; respawn cannot reuse an old process's ready record.
  `sequence` remains the separate per-process frame counter. Take a cursor
  **before** an action, then await its `rendered` event before capture. A control
  barrier alone does not prove that earlier tmux key input has been consumed.
- Set `config.manual_clock = true` to hold working/clearing phases without a
  duration race. Model time starts at zero; `control(pane, json!({"command":
  "advance_to", "now_ms": 500}))` advances it explicitly and acknowledges
  completed effect dispatch. Await `rendered` separately for the terminal.
  Time must be a nondecreasing integer. `{"command":"barrier"}` only acknowledges
  effects already processed; controls never become editor input. Autonomous
  model completion is disabled, not delayed. The selector blocks on input,
  controls and actual deadlines (only lone Escape needs a 20ms ambiguity timer).
- Legacy daemon-row/shell waits still poll at 20ms with 10-second guards; their
  subscription/process-exit migration is separate. Existing automatic-clock
  tests still use configured observation windows. New scenarios should hold
  phases with manual time instead of relying on those windows.
- `wait_for_row`, `wait_for_absent`, `pane_rows`, `rows` query the real daemon.
  SessionView has no PID: use owned panes/IDs, never total host row counts.
  `discover()` can see unrelated procfs processes. A pending row becoming one
  real-ID row proves discovery and the hook agree on PID.
- `capture(pane)` joins wrapped screen lines; `capture_plain(pane)` preserves
  viewport line structure and trailing newlines (`capture-pane -p`, no `-J`).
  `paste(pane, text)` uses bracketed
  paste plus a separate Enter. `key(pane, "Escape")` sends a key to the private
  server only. For an unsubmitted paste use `tmux` load/set-buffer + paste-buffer
  without Enter; typed and pasted text are visible in the active input line.
- `atm(args)` and `tmux(args)` return bounded command output, including refusals.
  `command(path)` supplies the same sealed environment to custom invocations;
  execute with `rig.run(&mut command)`. `tmux` supplies the private `-L` label.
  If also using role support, import `stand_in::bounded` rather than including
  `support/role.rs` twice.
- `journal(pane)` is diagnostic JSONL: identity, accepted/rejected/queued input,
  event/dropped, status, draft, render, ready, selection, connected/reconnecting.
  Render entries carry a per-process `sequence`. A subsequent `rendered` entry
  acknowledges tmux's cursor-position reply to a DSR query following that frame;
  `ready` follows only for the latest idle frame, not merely a stdout flush.
  Replies for superseded frames are discarded; replies never trigger a redraw.
  Frame output is deferred during fragmented paste/escape sequences. Exit stops
  input processing and discards pending frame acknowledgements, so no later
  render/ready can describe the exited process. These are rig milestones, not
  evidence of empty input or confirmed hook delivery. The same records are sent
  over `ATM_STANDIN_EVENTS`, a private Unix stream, before any model announcement.
  Every entry includes process identity and model time (manual when configured). Events/status are **send attempts**,
  not acknowledgments. Hook exit zero cannot prove delivery. Timeout diagnostics
  include journals, daemon logs and panes, and name possible lost handshakes.
- `close()` checks cleanup; Drop is the unwind fallback. Cleanup discovers every
  `atmd` with an environment path inside this rig, including CLI-auto-started
  daemons, rechecks procfs start time/environment, and stops it before removing
  state. The canonical tmux socket is recorded before fallible startup queries;
  cleanup uses that socket and rechecks its identity, including with symlinked
  TMPDIR. No broad process-name kill is used.

### Daemon control

`stop_daemon()`, `start_daemon()`, and `restart_daemon()` retain the same private
socket, state and tmux server. Start waits for a real list-sessions round trip.
Stop also finds detached daemons. With the daemon stopped, `rig.atm(...)` can
exercise the real CLI auto-start path, which cleanup owns too.

`set_daemon_env(key, value)` sets extra environment for **subsequent explicit
starts**, e.g. set `ATM_SPAWN_BIN` to `rig.launcher(mode)` and then restart. It
cannot override sealing variables or TMUX/PANE. It does not change environment
inside an already-running daemon or in CLI-auto-started daemons; for the latter,
set the extra variable on `rig.command(...)` before invoking the CLI.

### Spawn, arguments, IDs and respawn

`rig.launcher(mode)` is an absolute executable symlink named `claude`, `codex` or
`pi`, suitable for `ATM_SPAWN_BIN` (or a per-harness override). It is not shipped
as a binary. Arguments are validated **before** identity/session announcement.
One positional initial prompt is supported; `--` makes a leading dash literal.
Harness options such as `--model X` are ignored. Unknown options accept an
optional following non-option value; `--name=value`, `--no-*`, `--offline` and
`--dangerously-skip-permissions` need no following value. This is a test parser,
not a complete vendor CLI grammar. Known value-taking flags reject missing
values. Use `--` before a prompt if an unknown boolean option precedes it.

For CLI spawn, configure the **private tmux server environment**, since spawned
pane shells inherit the server rather than the CLI's environment:

```rust,ignore
let log = rig.root().join("spawn.jsonl");
for (key, value) in [
    ("ATM_STANDIN_LOG", log.to_str().unwrap()),
    ("ATM_STANDIN_ID_PREFIX", "spawn"),
    ("ATM_STANDIN_WORK_MS", "1200"),
] {
    assert!(rig.tmux(&["set-environment", "-g", key, value]).status.success());
}
let mut cmd = rig.command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
cmd.args(["spawn", "--target-pane", "%0", "--model", "fixture-model",
          "--prompt", "one quoted prompt;"])
   .env("TMUX", rig.tmux_env()) // private server, for CLI's in-tmux guard
   .env("ATM_SPAWN_BIN", rig.launcher(StandInMode::Claude));
let out = rig.run(&mut cmd);
assert!(out.status.success(), "{out:?}");
let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
let pane = rig.observe_pane(id, StandInMode::Claude, log);
// observe_pane waits for live identity, not idle: initial prompt may be working.
```

Current `atm spawn --prompt` rejects control characters: use single-line prompts
there and multiline `atm send` for delivery tests.

Each process adds a fresh UUID nonce: IDs are `<prefix>-<nonce>-<generation>`.
Generation starts at 1 and changes on clear. Treat IDs as opaque; read identity,
render, or daemon rows, never splice suffixes. A respawn with the same prefix has
new IDs. `observe_pane` selects the newest live identity, checking procfs start
time as well as PID; readiness ignores old-process renders.

`launch` writes `pane.log.with_extension("config.json")` and passes
`--atm-config <path>` in the original pane command. `respawn-pane -k -t <pane>`
therefore retains configuration without shell exports. A retained-shell relaunch
can use `pane.launcher --atm-config <that-path>`. The file has `LOG`, `ID_PREFIX`,
`WORK_MS`, `CLEAR_GAP_MS`, `MANUAL_CLOCK` (0/1) keys; it and the log/socket must resolve inside
`ATM_STANDIN_ROOT`. Defaults for the timers are 500ms/100ms; negatives fail before
announcement. Direct launch also requires the private TMUX/PANE and root; never
unset ATM_SOCKET to use a default.

## Controls and fidelity

| Mode | Startup | Prompt / completion | Clear | Interrupt |
|---|---|---|---|---|
| Claude | SessionStart + status | UserPromptSubmit / Stop + status | `/clear`: SessionEnd, gap, new SessionStart(source=clear) + status | no ending hook; restores interrupted prompt as draft |
| Codex | **no event** | first prompt in each generation: SessionStart then UserPromptSubmit; Stop completes | `/new`: fresh editor/ID internally, **no event until next prompt** | no ending hook |
| pi replay | session_start | input + agent_start / turn_end + agent_end | `/new`: shutdown, gap, new start on **same connection** | turn_end + aborted agent_end |

Codex discovery may create `pending-{pid}` and its old row remains across clear.
Waiting for its new-ID hook before the next prompt deadlocks: wait for the new
render ID/editor instead. Pi turn_end reports usage; agent_end ends work.

- `/atm/dialog` (Claude only) sends PreToolUse and PermissionRequest and displays
  the captured permission question/Yes/No/Esc screen. Up/Down select, Enter records
  a 1-based `selection.option`, Escape denies. Selection emits no hook and executes
  nothing (Synthetic: safe selection, not actual tool approval). Denial erases the
  dialog but leaves the daemon attention_needed. `atm reply --option 2` and
  `peek --prompt` exercise the real parser/key path.
- `/atm/drop-next [EVENT]` suppresses the next matching hook, or literally the next
  hook if omitted. Names are case-sensitive; invalid names don't arm loss. Status
  frames are separate and do not consume this hook-drop control.
- `/atm/status [ID]` (Claude only) forwards the captured-shaped status payload for
  the current or supplied ID, including during clear. Use an ended ID to exercise
  late-status handling. Synthetic: this control and automatic status timing are
  deterministic, not real latency or usage samples. Other modes emit no status.
- `/atm/exit`, `/exit` (Claude), `/quit` (Codex/pi) emit end and return to the shell.
  Codex ends all generations. Tests use `/atm/exit` for cleanup.
- Escape and Ctrl-C interrupt a running turn without exit; scheduled completion
  is cancelled. Claude restores the interrupted prompt as a visible draft (U5 C5).
  At idle Ctrl-C clears the draft; a second within one second exits. Ctrl-U clears
  the draft without submitting it. Up/Down are ignored outside dialogs.
- Claude submits busy input into a FIFO and emits UserPromptSubmit **only when the
  preceding turn ends**. This reproduces late confirmation/double-delivery risks.
  Clear discards queued input. Synthetic: queuing was not captured by U5; abort
  while a queue exists is not a faithful queue-cancellation model (pending entries
  remain until another turn completes). Codex/pi busy input is still rejected.
- Whole-line controls only: multiline text containing a control is an ordinary
  prompt. Input during the clear gap is rejected and journalled, not submitted.
- Bracketed paste handles fragmented UTF-8/delimiters. CR becomes LF as in normal
  tty input. Lone ESC resolves after a 20ms gap; CSI arrows are parsed atomically.
  This is not a full editor/terminal emulator (no cursor editing/history).
- Claude/Codex use `/bin/sh -c` to run the real repository hook scripts and socat,
  exercising their parent-process walk; PID/pane fields aren't injected.
- Pi **replays envelopes**, never loads pi-atm. One socket spans `/new`; after
  disconnect it retries and buffers unsent frames while the daemon is down.
  Buffering/backoff are synthetic, not an SDK transport conformance test. As with
  real unacknowledged writes, a disconnect during a write can lose/duplicate a
  frame; process exit does not guarantee flushing while the daemon is unavailable.

Remaining gaps: no Codex/pi native dialogs, trust/update prompts, permission
Notification hook or post-denial status timing, provider errors/resumptions,
real tokens/usage evolution, subagents, tool execution, or paid model calls.
Input-line rendering is deliberately simple, not a pixel-perfect vendor TUI.

## Captured templates

Captured: Claude Code 2.1.325, 2026-11-14, U5 real TUI through a timestamped private
Unix proxy into atmd. `claude_2.1.325.jsonl` selects source lines
1, **2 (status)**, 3, 4, 6, 7, 14, 15, 18, respectively. The adapter's
`claude_tui_session_2.1.310.jsonl` independently confirms lifecycle shapes.

Captured: Codex 0.167.0, 2026-11-14, U5 real TUI with `--no-daemon` through the same
proxy. `codex_0.167.0.jsonl` selects lines 3, 4, 5, 12.

Captured: pi 1.5.0, 2026-11-14, U5 real TUI with pi-atm through the same proxy.
`pi_1.5.0.jsonl` selects lines 1, 2, 3, 5, 6, 7, 8, 18, 19. agent_end retains only
the assistant; system/tools/user context and signed text metadata are omitted.
The capture carries `/new` on one persistent connection.

Sanitization: paths become `/fixture/...`; identifiers become `fixture-*`;
timestamps become zero. PID/pane are removed for the real forwarder to derive.
Model names, source/reason/stopReason and usage shapes are retained. The permission
screen is an excerpt with marker text replaced. Synthetic: runtime IDs, paths,
prompt and timing substitutions construct controlled scenarios, not new captures.
