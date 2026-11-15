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
  SessionView has no PID: identify rows by owned panes/IDs. The rig sets a
  private `ATM_TMUX_SOCKET` label: startup discovery and `discover()` admit only
  processes mapped to that server's panes, and admit none if pane listing fails.
  This supersedes the initial M1 allowance for foreign pending rows documented
  in `scripts/atm-candidate.md` under "Lifecycle and limits": U11b's concurrent
  runs proved host processes fill the 100-row registry and leak host sessions
  into failure logs. Default-label daemons remain host-wide; private tests that
  need discovery isolation must set a label in their own helpers. Direct hooks
  are not filtered. A pending row becoming one real-ID row proves discovery
  and the hook agree on PID.
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
  `wait_for_shell` additionally awaits the retained bash prompt's event, keyed by
  the exited harness PID/start time so a prior prompt cannot satisfy a relaunch.
  The rig's `PROMPT_COMMAND` prints synthetic `stand-in-shell$` chrome and waits
  for tmux's cursor-position reply before publishing that event; `PS1` is cleared
  in the hook itself (bash resets an inherited empty PS1). New shells without an
  exited harness do not read a cursor reply, leaving queued spawn input to bash.
  Send nothing to a retained shell until `wait_for_shell` returns.
  Verified with tmux 3.8 and bash 5.3.20; no model CLI is involved.
  `command(path)` supplies the same sealed environment to custom invocations;
  execute with `rig.run(&mut command)`. `tmux` supplies the private `-L` label.
  If also using role support, import `stand_in::bounded` rather than including
  `support/role.rs` twice.
- `stand_in_forwarder_identity` names its failed step and dumps its journal,
  fresh rows and daemon diagnostics. It enables `ATM_STANDIN_TRACE_HOOKS=1` on
  its private server: real hook bash xtrace goes to the pane's `.hook-trace.log`,
  including previous command exit status, without enabling `/tmp/atm-hook.log`.
  This observes send attempts, not delivery guarantees, and adds tracing overhead.
  Failure diagnostics can include discovered host sessions; keep them local.
  Concurrent loaded runs reproduced a Codex pending-row timeout before any hook:
  daemon logs rejected that exact PID at the registry's 100-session cap. Private
  sockets do not isolate `/proc` discovery from other rigs or host harnesses.
  Run a failing binary alone and report both results; do not extend waits or
  infer hook loss from a row timeout. A socat timeout status alone also does not
  prove a dropped hook (the daemon may already have accepted its messages).
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
as a binary. The pi stand-in overwrites its own `/proc` cmdline with the bare
name `pi` and NUL padding once it has started, as a real pi's
`process.title = "pi"` does (pi 1.5.0); the Claude and Codex stand-ins keep the
launcher path there. Arguments are validated **before** identity/session announcement.
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

### One-shot paste refusal (synthetic)

Use the existing private control/event stream, not editor text:

```rust,ignore
rig.control(&pane, serde_json::json!({"command": "refuse_next_paste"}));
let after = rig.event_cursor(&pane);
rig.paste(&pane, "this input is not taken");
rig.wait_event(&pane, after, |e| e["kind"] == "rejected"
    && e["reason"] == "refuse_next_paste");
```

The arm acknowledgment precedes the action; the `rejected` event (with `text`)
proves the entire next bracketed paste was consumed without entering the editor,
model, queue, slash commands or dialog selection. Fragmented/multiline input is
one paste. Its immediately following Enter is swallowed so an older draft is not
submitted; the draft is preserved. New ordinary typing or another paste resumes
normal input. Cursor reports, control messages, arrow keys and lone Escape do not consume that Enter.
Ordinary typing/CLI prompts do not consume the arm. A duplicate arm, an unfinished
paste/escape sequence, or a refusal still awaiting its Enter rejects arming without
changing the existing input/arm. The next paste after the rejected submission is
normal. No fake dialog or readiness frame is drawn.

This is **harness non-acceptance after tmux pasted bytes**, not proof that the
sender sent nothing. It cannot justify a known-zero-effect automatic retry or
unignore `message_delivery::safe_retry_after_known_zero_effect`.

### Releasable hook hold (synthetic)

The same channel holds hooks without holding model time, terminal rendering or
control processing. Arm only one gate per process:

- `{"command":"hold_next_hook"}` holds the next emitted lifecycle hook.
- `{"command":"hold_next_hook","event":"Stop"}` holds the next matching hook.
  Names are case-sensitive and must exist in this mode's captured templates;
  omission/JSON null means literally next. For pi's actual end use `agent_end`,
  or omit `event` to hold both completion hooks in order.
- A `kind=held` runtime event, with `event` and the exact `data`, is the fence that
  the hook was withheld, **not** sent. It keeps its original session ID. Every
  later lifecycle hook is also held in emission order, including across clear.
  Thus pi's `agent_end` cannot overtake a held `turn_end`. Status payloads are
  independent: neither held nor a consumer of the arm. Claude statuses can therefore
  precede a held Stop or a new session's held SessionStart.
- `{"command":"release_hook"}` forwards the buffered hooks exactly once, in
  order, through the normal hook/replay path, then acknowledges the control.
  It disarms the gate; later hooks flow normally. An acknowledgment is still only
  completed forwarding attempts, not daemon receipt: await the daemon row/event
  when receipt matters. A held shutdown also defers process exit until release,
  keeping this control stream usable; exit otherwise behaves as before. An unfired
  literal-next arm also holds `/atm/exit`'s SessionEnd/session_shutdown: use a named
  completion arm instead, or await that shutdown's `held` event and `release_hook`
  before `wait_for_shell`. Closing the pane (stdin EOF) or terminating the process
  with a signal drops held hooks and any queued shutdown hook; none are replayed.
- A duplicate arm (armed or holding), unknown/malformed name, or release with no
  captured hook returns a control error without overwriting the gate/draft.
  Release while only armed does not cancel that arm. Existing `/atm/drop-next`
  takes precedence: a dropped hook neither consumes the hold nor reappears on
  release, including when another hook is already held.

For a late end, start a turn on manual time and await its Working row, then:

```rust,ignore
// Claude/Codex: optionally add "event": "Stop"; pi: omit it to hold both ends.
rig.control(&pane, serde_json::json!({"command": "hold_next_hook"}));
let after = rig.event_cursor(&pane);
rig.control(&pane, serde_json::json!({"command": "advance_to", "now_ms": 500}));
rig.wait_event(&pane, after, |e| e["kind"] == "held");
rig.wait_event(&pane, after, |e| e["kind"] == "rendered" && e["state"] == "idle");
// The editor looks idle but the completion hook has not been forwarded.
rig.control(&pane, serde_json::json!({"command": "release_hook"}));
rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Idle);
```

Here `500` is the default `work_ms` from an initial manual clock of zero; use the
turn's actual deadline if the test has advanced it. This is synthetic fault
injection, not a claim about vendor hook scheduling. No sleeps are needed.

### Next context share (synthetic)

`{"command":"set_context_share","percent":40}` on the same duplex channel
acknowledges a **one-shot** setting for the next generated context-bearing
payload. It does not send a payload, advance time, end a turn or redraw. `percent`
is a finite JSON number from 0 through 100 inclusive; booleans, null, missing,
strings and out-of-range values return a control error without changing a pending
setting/draft. Another valid setting replaces an unused setting. Tokens round to
the nearest integer (ties to even); fractional shares can differ by half a token
from the requested value. Unconfigured payloads and captured templates are unchanged.

| Mode | Next eligible report | Fields changed / window |
|---|---|---|
| Claude | Next automatic status or `/atm/status [ID]` | `context_window.used_percentage`, `remaining_percentage`, `current_usage.input_tokens` with other current/cache fields zero, and matching one-request totals; captured `context_window_size` is 1000000 |
| pi | Next usage-bearing `turn_end` envelope, including abort | `usage.contextTokens` over captured `model.contextWindow` 272000; cost/session-token snapshot stays unchanged |
| Codex | Next non-ending lifecycle hook | Its `transcript_path` points to a new private JSONL `event_msg`/`token_count` record: `info.last_token_usage.total_tokens` and matching `total_token_usage.total_tokens`, over `model_context_window` 258400 |

Claude's daemon adapter **calculates** context from current input/cache tokens;
changing `used_percentage` alone would not move its line. Codex has no usage/status
hook field, and the daemon skips transcript usage at `SessionEnd`, so that event
does not consume the setting. Each synthetic Codex record is written before its
referencing hook is forwarded, at a unique process/session/sequence path inside
the rig root. Later settings never overwrite that file, including for held hooks.
A subsequent unconfigured Codex hook reports no new usage, so the daemon keeps
its last known share; use another setting (including zero) and a non-ending hook
to reset it. Claude/pi return to their captured payload values after consumption.
Real Codex reuses one rollout per session; this synthetic control switches paths,
and unconfigured hooks reference an absent fixture transcript instead of rereading it.
Synthetic pi `contextTokens` can exceed the unchanged session-token total; this
control injects a context line, not internally consistent usage accounting.

Consumption is at **generation**, not receipt: a dropped report consumes the
setting; a held report keeps its generated value even if a new setting is armed
before release. Ordinary non-usage pi events do not consume it. A pending setting
is not a lifecycle end or a readiness signal. Await the real daemon row for the
reported share; its control acknowledgment alone is not proof of receipt:

```rust,ignore
// After starting a manual-clock turn and awaiting its Working row:
rig.control(&pane, serde_json::json!({"command":"set_context_share", "percent":40}));
rig.control(&pane, serde_json::json!({"command":"advance_to", "now_ms":500}));
rig.wait_for_row(&pane, |r| r.status == atm_core::SessionStatus::Idle
    && (r.context_percentage - 40.0).abs() < 1e-6);
```

As above, use the actual work deadline rather than 500 if time already advanced.
Use a tolerance: the daemon computes an f64 quotient, so even an integer share
such as 58 can be reported as 57.99999999999999 rather than exactly 58.0.
**Synthetic:** context counts/percentages and the Codex rollout composition are
fault-injection values, not captured usage measurements. Codex's record shape and
258400 capacity reuse the captured `codex_rollout_tokens_0.163.0.jsonl` documented
by `atm-codex-adapter`, composed here with the stand-in's 0.167.0 hook templates;
this is not a claim of a new Codex capture. No cost, terminal capture or production
parser is changed.

### Lifecycle fidelity

| Mode | Startup | Prompt / completion | Clear | Interrupt |
|---|---|---|---|---|
| Claude | SessionStart + status | UserPromptSubmit / Stop + status | `/clear`: SessionEnd, gap, new SessionStart(source=clear) + status | no ending hook; restores interrupted prompt as draft |
| Codex | **no event** | first prompt in each generation: SessionStart then UserPromptSubmit; Stop completes | `/new`: fresh editor/ID internally, **no event until next prompt** | no ending hook |
| pi replay | session_start | input + agent_start / turn_end + agent_end | `/new`: shutdown, gap, new start on **same connection** | turn_end + aborted agent_end |

Codex discovery may create `pending-{pid}` and its old row remains across clear.
Waiting for its new-ID hook before the next prompt deadlocks: wait for the new
render ID/editor instead. Pi turn_end reports usage; agent_end ends work.

- `/atm/dialog` sends captured PreToolUse and PermissionRequest **before** the
  Claude permission screen or optional Codex three-option modal appears (Codex
  requires `permission_dialog = true` and announces the generation if needed).
  Up/Down select; Enter records a 1-based `selection.option`. Claude option 1 and
  Codex options 1/2 approve: replay PostToolUse immediately and show working;
  Stop stays at the work deadline. P1 observed PostToolUse shortly after Enter,
  before Stop, so the daemon row leaves attention_needed during that interval.
  Synthetic: tool execution is represented by its
  captured completion, never by running arbitrary shell commands. No/Enter and
  Escape retain the denial/interruption screens and emit no completion hooks;
  the daemon may remain attention_needed. `atm reply --option 2` for Claude and
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
  EOF triggers a 1000ms close backoff on monotonic transport time, never the
  manual model clock; sends inside it are dropped, not replayed. Timer expiry
  emits `transport_retry_ready` for condition-based test synchronization but
  never reconnects: a later send
  initiates the next connection. With no socket outside backoff, a 256-entry
  drop-oldest FIFO waits for a later send. A write attempt is unacknowledged and
  is not retried as if delivery were known. The daemon-restart test awaits EOF
  and the backoff-expired event, then sends a new event; pure supplied-clock
  tests prove backoff drops without racing real input against a one-second timer.
  Connect and outbox writes never wait for a handshake reply, matching pi-atm.
  This models the repository pi-atm send/close/outbox rules, not SDK conformance:
  Python performs bounded synchronous connect/write, not Node's
  pending-socket/drain callbacks or high-water-mark behavior. Process exit does
  not guarantee flushing while the daemon is unavailable.

Remaining gaps: pi native selectors, permission Notification hook or post-denial
status timing, provider errors/resumptions, real tokens/usage evolution, subagents,
tool execution, or paid model calls. Input editing and uncaptured viewport sizes
are synthetic, not a complete vendor TUI.

## Captured screens and optional dialogs

The runtime renders `screens.json`'s 76 source variants through the pure
`screens.py` renderer and `scene.py` lifecycle mapping. Sources reuse adapter plain
captures, 20 U5b SGR/cursor pairs, and 16 sanitized P1 captures (provenance in
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
before launch. A stand-in draws at its launch size: do not resize or split its
pane after launch (split another pane instead). SIGWINCH is not handled and the
minimum size is not rechecked after a resize. `screen_variant` defaults to `u5`; choose `rich-enabled`/`poor`
for Claude or `rich`/`poor` for Codex at 100x30. Native geometry wins over a variant
from another size. `screen_variant = "values"` (JSON `SCREEN_VARIANT`) selects
Synthetic value-only variation: equal-width version/model substitutions, another
Claude/pi spinner, and nonzero pi cost on nonfresh frames. Fresh pi usage stays
zero; drafts/folder input are inserted afterwards and never rewritten. Source
replay bypasses mutations and still reproduces the original captures. Contract
tests pin unchanged facts across the variants, not particular scalar literals.
P1 trust has 80/120/200x40, and Codex dialogs 100x24/32 sources.
Uncaptured combinations are explicitly synthetic: redundant blank padding is
removed before old transcript, preserving editor blanks and history separators.
Pi rules reflow to equal widths and aligned usage/model and Codex shortcut/warning
footers retain complete tokens. Claude's standalone effort and `Ctrl+Y to paste
deleted text` lines are dropped whole if they would be clipped. Codex fresh panes below 25 rows use P1's compact
100x24 header/editor/footer capture (a18), not the tall logo, which needs 25 rows.
This is a source splice from P1's gpt-5.4 session into the U5 lifecycle; resizing
that capture to other compact sizes is Synthetic. The adapter contract pins
all lifecycle facts at 100x30, 80x24 (Codex), 100x24, 120x19 and the supported minima: **Codex 80x19,
pi 86x19**. Claude requires **53 columns** (its existing two-row floor is unchanged,
not a claim of lifecycle fidelity at that height); fresh facts are pinned at
53x19/24/30/40. A 52-column Claude pane or 60-column Codex/pi split is too narrow
for captured chrome: scene rendering explicitly refuses it. Only `launch`, `wait_ready` and `wait_event`
surface the scene error immediately rather than waiting for a misleading ready
frame; `observe_pane` and `wait_for_row` do not surface scene errors. Use a wider pane for these modes.
Source-only replay remains native-size evidence. This is not real-harness resize
fidelity. Other long transcript lines may clip; drafts cell-wrap.
Optional `config.folder` substitutes
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
terminal screen on exit; pi retains history and puts the shell below it. Claude
follows U5's clean-shell capture (without its resume hint); P1 captured the same
2.1.325 version leaving the UI inline above the shell instead. Exit-display
fidelity is therefore not universal. P1 also saw the first quick Down/Enter on
Claude trust reset to No; the stand-in models stable selection, not that reset.
No frame/ready event is emitted after exit. Codex hook-review Esc and permission
`y`/`p` shortcuts are not modeled: use the documented arrows/Enter or modal Escape.

Pure source/scene/key tests run through the Cargo Python contract; private tmux
integration tests cover native fresh replay, modal keys and retained shells.
`stand_in_screens` calls all three real adapters, not copied parsers. It compares
all 76 sources through private tmux with independent source text and pinned legacy
`observe_screen` facts; 20 styled pairs compare dim/reverse attributes and cursor-space cells,
allowing tmux to normalize SGR encodings. Separate key-driven tests cover drafts,
working/idle, each clear phase, repeated identical clear snapshots, interruptions,
actual retained shells, trust, update, hook review/list, worktree and permission.
P1 Codex dialogs and selected Claude trust remain Unknown to legacy `observe_screen`.
In chunk A, B0's `observe_control_screen` recognises these modals; the stand-in tests do not pin it.
Claude's rich identical-draft source has a separate warning footer and is wholly
Unknown to legacy `observe_screen`, not merely unknown input. No parser special cases are added.

For supplementary source-only conformance, `control(pane, json!({"command":
"render_source", "key": "claude.permission.u5"}))` replays a catalog entry for
that pane's mode. Set the private window to its catalog geometry first and hold
model time. Await `rendered` with `state == "source-replay"`; this uses the same
renderer and terminal barrier but does not change the model or emit `ready`.
Unknown/cross-mode keys and replay during incomplete terminal input/paste refuse.
This diagnostic control never proves a lifecycle
transition; the key-driven tests are the independent transition evidence.

A real-looking placeholder, selected trust option or Codex draft may still be
Unknown to legacy `observe_screen`. Neither the journal nor a rendered frame is
permission to deliver a prompt.
A rig daemon discovers every harness process on the host (private sockets do not
isolate `/proc`), so concurrent runs can fill its registry (`Registry is full`,
max 100) and failure output can name the host's real sessions; a separate unit
addresses discovery scope.

## Captured templates

Captured: Claude Code 2.1.325, 2026-11-14, U5 real TUI through a timestamped private
Unix proxy into atmd. `claude_2.1.325.jsonl` selects source lines
1, **2 (status)**, 3, 4, 6, 7, 14, 15, 18, respectively. The adapter's
`claude_tui_session_2.1.310.jsonl` independently confirms lifecycle shapes.

Captured: Codex 0.167.0, 2026-11-14, U5 real TUI with `--no-daemon` through the same
proxy. `codex_0.167.0.jsonl` selects lines 3, 4, 5, 12.

Captured: Claude Code 2.1.325 and Codex 0.167.0, 2026-11-14, P1 private proxy
permission probes. Claude's appended PostToolUse is raw record 52; Codex's
appended PreToolUse, PermissionRequest and PostToolUse are raw records 39–41.
They preserve payload shapes and tool results, with the identifier/path
sanitization below. These are source splices, not one continuous captured tool
flow: Codex permission events carry P1's gpt-5.4 rather than U5's gpt-6-luna,
so the daemon's model changes during permission handling. Claude's P1 PostToolUse
closes U5's PreToolUse/PermissionRequest with a matching sanitized tool_use_id but
different tool_input and permission_mode. The stand-in replays these events
rather than executing tools.

Captured: pi 1.5.0, 2026-11-14, U5 real TUI with pi-atm through the same proxy.
`pi_1.5.0.jsonl` selects lines 1, 2, 3, 5, 6, 7, 8, 18, 19. agent_end retains only
the assistant; system/tools/user context and signed text metadata are omitted.
The capture carries `/new` on one persistent connection.

Sanitization: paths become `/fixture/...`; identifiers become `fixture-*`;
timestamps become zero. PID/pane are removed for the real forwarder to derive.
Model names, source/reason/stopReason and usage shapes are retained. Permission
screens use the full catalog captures; synthetic selection changes only the
selected marker. Synthetic: runtime IDs, paths, prompt and timing substitutions
construct controlled scenarios, not new captures.
