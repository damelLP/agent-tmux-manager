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
config.manual_clock = true; // hold phases until the test releases them
config.work_ms = 1200;
config.clear_gap_ms = 1200;
let pane = rig.launch(&config); // current-process terminal-acknowledged first frame
let old = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
assert!(rig.atm(&["send", &pane.pane_id, "one\ntwo"]).status.success());
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
rig.control(&pane, serde_json::json!({"command":"advance_to", "now_ms":1200}));
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
rig.wait_ready(&pane, Some(old.id.as_str()));
let after = rig.event_cursor(&pane);
rig.paste(&pane, "/clear");
rig.wait_event(&pane, after, |e| e["kind"] == "rendered" && e["state"] == "clearing");
rig.wait_for_absent(&old.id);
rig.control(&pane, serde_json::json!({"command":"advance_to", "now_ms":2400}));
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
  `sequence` remains the separate per-process frame counter. Frame records also
  carry `frame` (scene), `source` (capture path), and the draft, so fragmented input can be awaited before capture, without
  polling for text. Take a cursor
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
- `wait_for_row` and `wait_for_absent` consume one persistent daemon subscription,
  whose initial snapshot is received before actions. Updates/removals notify
  waiters after the cached committed state changes; no list-sessions polling.
  Restart creates a fresh subscription. `pane_rows` and `rows` make explicit
  snapshot queries to the real daemon.
  SessionView has no PID: use owned panes/IDs, never total host row counts.
  `discover()` can see unrelated procfs processes. A pending row becoming one
  real-ID row proves discovery and the hook agree on PID.
- `capture(pane)` joins wrapped screen lines; `capture_plain(pane)` preserves
  viewport line structure and trailing newlines (`capture-pane -p`, no `-J`).
  `capture_styled(pane)` uses `capture-pane -e -N -p`, retaining SGR and trailing
  cells. This is a renderer test seam, not a styled observer API.
  `paste(pane, text)` uses bracketed
  paste plus a separate Enter. `key(pane, "Escape")` sends a key to the private
  server only. For an unsubmitted paste use `tmux` load/set-buffer + paste-buffer
  without Enter; typed and pasted text are visible in the active input line.
- `atm(args)` and `tmux(args)` return bounded command output, including refusals.
  Commands and `wait_for_shell` wait on Linux pidfd exit notification with a
  30-second hang guard, not repeated `try_wait`/procfs polling. Command output
  goes through temporary files so large output cannot fill a pipe.
  `command(path)` supplies the same sealed environment to custom invocations;
  execute with `rig.run(&mut command)`. `tmux` supplies the private `-L` label.
  If also using role support, import `stand_in::bounded` rather than including
  `support/role.rs` twice.
- `journal(pane)` is diagnostic JSONL: identity, accepted/rejected/queued input,
  event/dropped, status, draft, render, ready, selection, connected/disconnected,
  transport_dropped.
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
  cleanup owns that canonical socket, not the first server PID. A replacement
  server at the same socket is accepted only after its private TMUX_TMPDIR,
  procfs start identity and returned socket/PID are checked and rechecked. An
  already-stopped server is clean only when no matching owned server remains;
  a live server with an unavailable socket is an error, not silently abandoned.
  `tmux_env()` queries the current identity after a restart. Daemon disappearance
  during a recheck is already stopped; a changed identity is never signalled.
  Both daemon and tmux cleanup are attempted even when the first fails. Process
  termination uses pidfd notifications, not cleanup polling. Symlinked TMPDIR is
  canonicalized for ownership; no broad process-name kill is used. Diagnostics
  also read the known detached-daemon path `state/atm/atm.log` without recursively
  scanning unrelated paths.
- The symlinked-TMPDIR subprocess test uses `parent::ChildGuard`, not the shared
  short command-timeout helper. The parent owns its base before spawn; the child
  reports its root, canonical socket and creator identity, then waits for an
  acknowledgment **before** starting event listeners/tmux/atmd. After acknowledgment
  the child's TempDir no longer deletes its directory: only the parent may do so.
  A second socket barrier holds the fully started child for normal finish or a
  deliberate forced exit. Parent cleanup uses the same procfs ownership rules,
  rechecks before signals/server shutdown, then removes the child root. Drop also
  handles parent assertion unwinding; cleanup failure retains the directory for
  diagnosis rather than deleting a live server's socket. Tests check directory
  retention after forced exit, so this path prevents the old missing-socket leak
  instead of relying on socket reconstruction. Deliberate external deletion of
  a guarded socket directory is not modeled.

### Daemon control

`stop_daemon()`, `start_daemon()`, and `restart_daemon()` retain the same private
socket, state and tmux server. Start waits for a real list-sessions round trip.
Stop also finds detached daemons. With the daemon stopped, `rig.atm(...)` can
exercise the real CLI auto-start path, which cleanup owns too.

`set_daemon_env(key, value)` sets extra environment for **subsequent explicit
starts**. It cannot override sealing variables or TMUX/PANE. It does not change
environment inside an already-running daemon or CLI-auto-started daemons; for
the latter set the extra variable on `rig.command(...)` before invoking the CLI.
**ATM_SPAWN_BIN is CLI-only at this tip**: setting it in the daemon's environment
does not change daemon command construction.

### Spawn, arguments, IDs and respawn

`rig.launcher(mode)` is an absolute executable symlink named `claude`, `codex` or
`pi`, suitable for `ATM_SPAWN_BIN` (or a per-harness override). It is not shipped
as a binary. Arguments are validated **before** identity/session announcement.
One positional initial prompt is supported; `--` makes a leading dash literal.
It remains pending through startup dialogs and is submitted once after they
finish, never interpreted as a trust/update selection. This is a synthetic safe
composition of the stand-in's CLI and dialog controls; the combined real-CLI
startup behavior has not been captured.
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

For a **daemon-built command**, put the explicit absolute `rig.launcher(mode)`
in the role/config command accepted by that consumer, with `--atm-config` pointing
to a private JSON file containing `LOG` and `ID_PREFIX`. Alternatively configure
those values in the private tmux server environment as above. Do not rely on
ATM_SPAWN_BIN or weaken the sealed PATH. For example, the command builder's argv
can be `[rig.launcher(mode), "--atm-config", rig.root().join("role.json")]`:

```rust,ignore
let config_path = rig.root().join("role.json");
std::fs::write(&config_path, serde_json::json!({
    "LOG": rig.root().join("role.jsonl"), "ID_PREFIX": "role", "MANUAL_CLOCK": 1
}).to_string()).unwrap();
let command = [rig.launcher(StandInMode::Claude).into_os_string(),
               "--atm-config".into(), config_path.into_os_string()];
// Supply command to the consumer's role/command builder, not ATM_SPAWN_BIN.
```

`stand_in_explicit_launcher_without_cli_spawn_override` exercises that executable
and config through a private tmux pane without the CLI override. It verifies the
launch route, not a future consumer's command builder.

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
`WORK_MS`, `CLEAR_GAP_MS`, `MANUAL_CLOCK` (0/1), `SCREEN_VARIANT`, `FOLDER`, and
optional dialog flags described below; it and the log/socket must resolve inside
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

- `/atm/dialog` in Claude sends PreToolUse and PermissionRequest and displays
  the captured permission question/Yes/No/Esc screen. Up/Down select, Enter records
  a 1-based `selection.option`, Escape denies. Selection emits no hook and executes
  nothing (Synthetic: safe selection, not actual tool approval). Denial erases the
  dialog but leaves the daemon attention_needed. `atm reply --option 2` and
  `peek --prompt` exercise the real parser/key path. Codex supports `/atm/dialog`
  only with `permission_dialog = true`: it renders P1's actual three-option modal,
  consumes Down/Enter or Escape, and executes nothing/emits no fabricated hooks.
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
- **Synthetic adversarial FIFO, not current Claude fidelity:** busy input is
  queued and emits UserPromptSubmit only when the preceding turn ends. P1's real
  Claude Code 2.1.325 capture (2026-11-14) instead observed accepted/queued prompt
  receipt **before** the held turn completed. Do not use this FIFO as evidence of
  real receipt timing, and never infer execution from receipt. Clear discards
  the synthetic queue; abort leaves entries until another turn completes, also
  not a faithful cancellation model. Existing queue tests deliberately retain
  this adversarial contract. Codex/pi busy input remains rejected.
- Whole-line controls only: multiline text containing a control is an ordinary
  prompt. Input during the clear gap is rejected and journalled, not submitted.
- Bracketed paste handles fragmented UTF-8/delimiters. CR becomes LF as in normal
  tty input. Lone ESC resolves after a 20ms gap; CSI arrows are parsed atomically.
  This is not a full editor/terminal emulator (no cursor editing/history).
- Claude/Codex launch `/bin/bash <real repository hook>` directly, matching U5's
  Claude Code 2.1.325/Codex 0.167.0 ancestry: socat ← timeout ← hook ← harness.
  A private Bash-startup barrier checks the actual child's PID/PPID; existing real
  daemon tests still check forwarded identity. PID/pane fields aren't injected.
- Pi **replays envelopes**, never loads pi-atm. One socket spans `/new`. Observed
  EOF triggers a 1000ms close backoff on the supplied model clock; sends inside
  it are dropped, not replayed. Advancing time never reconnects: a later send
  initiates the next connection. With no socket outside backoff, a 256-entry
  drop-oldest FIFO waits for a later send. A write attempt is unacknowledged and
  is not retried as if delivery were known. The daemon-restart test awaits EOF,
  proves outage input dropped, then sends a new event after explicit clock
  advancement. It never asserts recovery of outage input.
  This models the repository pi-atm send/close/outbox rules, not SDK conformance:
  Python performs bounded synchronous connect/handshake/write, not Node's
  pending-socket/drain callbacks or high-water-mark behavior. Process exit does
  not guarantee flushing while the daemon is unavailable.

Remaining gaps: pi native selectors, permission Notification hook or post-denial
status timing, provider errors/resumptions, real tokens/usage evolution, subagents,
tool execution, or paid model calls. Input editing and uncaptured viewport sizes
are synthetic, not a complete vendor TUI.

## Captured screens and optional dialogs

The runtime renders `screens.json`'s 74 source variants through the pure
`screens.py` renderer and `scene.py` lifecycle mapping. Sources reuse adapter plain
captures, 20 U5b SGR/cursor pairs, and 15 sanitized P1 captures (provenance in
`fixtures/screens/README.md`). No runtime ID or diagnostic status text is visible.
`frame` records the model's fresh, working, idle after a turn, clearing,
clear-complete, interrupted and modal phases; `source` identifies the actual
capture chosen, including draft/completion-menu substitutions. Claude clear shows the real SessionEnd hook spinner;
pi retains a captured old conversation; Codex can show the captured blank
transition. Completed clear uses genuine fresh captures, not the misleading
post-turn `after-clear` fixtures. These scenes are snapshots, not transcript replay.

Native output preserves rows, NBSP and padding. Trusted SGR is validated against
plain text; styled cursor spaces survive even when plain capture drops them.
Cursor sidecars retain evidence for a later cursor-aware renderer/observer, but
hardware cursor coordinates are not restored to those captures; current adapter
conformance passes `None` for cursor. No cursor-placement fidelity is claimed.
Absolute row positioning avoids bottom-row newline scrolling. Synthetic runtime
drafts replace only the active editor slot and render control bytes visibly.
Capture-specific draft styling is preserved for an exact captured draft; other
drafts use normal attributes. Modal text never becomes an editor draft. Diagnostics
and suppressed hooks remain in event records, not invented screen chrome.

`config.columns/rows` default to 120x40 and establish the private window size
before launch. `screen_variant` defaults to `u5`; choose `rich-enabled`/`poor`
for Claude or `rich`/`poor` for Codex at 100x30. Native geometry wins over a variant
from another size. P1 trust has 80/120/200x40, and Codex dialogs 100x24/32 sources.
Uncaptured combinations are explicitly synthetic: blank padding is removed first,
then leading transcript if needed; long chrome is clipped, drafts cell-wrapped.
This is not real-harness resize fidelity. Optional `config.folder` substitutes
only the trust path (control bytes escaped); omission preserves the capture.

All dialog flags default false, survive respawn, and reject incompatible modes
before identity or lifecycle announcement. Config JSON/environment flags use
`FOLDER_TRUST`, `UPDATE_PROMPT`, `WORKTREE_CHOICE`, `HOOKS_REVIEW`,
`PERMISSION_DIALOG` with 0/1 values (environment prefix `ATM_STANDIN_`).

| Flag | Modes | Keys and behavior |
|---|---|---|
| `folder_trust` | Claude | Default No; Down selects Yes, Enter accepts and only then emits SessionStart. No/Enter or Esc exits without lifecycle hooks. |
| `folder_trust` | Codex | Default Trust and continue; Enter advances. Option 2/Back is explicitly refused in place: Command Center is not modeled. |
| `update_prompt` | Codex | Esc skips for this launch; update execution is refused. |
| `hooks_review` | Codex | Enter opens the captured hook list; `t` trusts, Esc returns. Down/Enter trusts all; option 3 continues but suppresses hooks as advertised. |
| `worktree_choice` | Codex | `/new` holds old generation at the modal; Enter on default commits clear, Esc restores old scene/ID. Creating a checkout is refused. |
| `permission_dialog` | Codex | Enables the safe `/atm/dialog` control described above. |

When combined, Codex start order is update → folder → hooks → editor. Pi has no
start dialog. `launch` returns on the first **rendered modal**, without waiting
for ready or a daemon row. Use `wait_event` after keys; no `ready` is emitted for
a modal. Selecting it cannot confirm delivery. Claude/Codex restore the normal
terminal screen on exit; pi retains history and puts the shell below it. No
frame/ready event is emitted after exit.

Pure source/scene/key tests run through the Cargo Python contract; private tmux
integration tests cover native fresh replay, modal keys and retained shells.
`stand_in_screens` calls all three real adapters, not copied parsers. It compares
all 74 sources through private tmux with independent source text and pinned U8a
facts; 20 styled pairs compare dim/reverse attributes and cursor-space cells,
allowing tmux to normalize SGR encodings. Separate key-driven tests cover drafts,
working/idle, each clear phase, repeated identical clear snapshots, interruptions,
actual retained shells, trust, update, hook review/list, worktree and permission.
P1 Codex dialogs and selected Claude trust remain Unknown under unchanged U8a.
Claude's rich identical-draft source has a separate warning footer and is wholly
Unknown, not merely unknown input. No parser special cases are added.

For supplementary source-only conformance, `control(pane, json!({"command":
"render_source", "key": "claude.permission.u5"}))` replays a catalog entry for
that pane's mode. Set the private window to its catalog geometry first and hold
model time. Await `rendered` with `state == "source-replay"`; this uses the same
renderer and terminal barrier but does not change the model or emit `ready`.
Unknown/cross-mode keys refuse. This diagnostic control never proves a lifecycle
transition; the key-driven tests are the independent transition evidence.

A real-looking placeholder, selected trust option or Codex draft may still be
Unknown to the current parsers. Neither the journal nor a rendered frame is
permission to deliver a prompt.

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
