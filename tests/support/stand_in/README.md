# Shared stand-in harness (test only)

Python standard library fixture for root integration tests. Never invokes a
model CLI or executes submitted text. Linux only (procfs process identity).

## Prerequisites and running

Required, never silently skipped: `python3`, `tmux` (with bracketed-paste state,
3.7+), `jq`, **real `socat`**, `timeout`, and `/bin/bash`. The rig resolves these
from `/usr/local/bin:/usr/bin:/bin`; no pip/npm packages are needed. Missing tools
fail with an install diagnostic. CI must provision them; the test does not install
packages or replace the transport. The repository hook scripts remain unmodified.

Run `cargo test --test stand_in_harness -- --nocapture` under your project's
sealed test environment. This includes Python unittest, so Cargo CI cannot omit
the parser/state-machine tests. Use Cargo's exact built binaries, not installed
`atm`/`atmd`. Full workspace runs still need the project's outer seal for older
tests; this new rig seals each subprocess independently.

## Consumer API (U7, U8, U9b)

Use ordinary synchronous `#[test]` functions. Protocol queries internally use a
short-lived Tokio runtime with a deadline; do not nest them inside an async runtime.
Include `#[path = "support/stand_in.rs"] mod stand_in;` in a root integration test:

```rust,ignore
use atm_core::SessionStatus;
use stand_in::{StandInConfig, StandInMode, StandInRig};

let mut rig = StandInRig::start();
let mut config = StandInConfig::new(StandInMode::Claude);
config.work_ms = 1200; // enough observation time even on a busy runner
let pane = rig.launch(&config); // waits for editor + bracketed-paste flag
let row = rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
let sent = rig.atm(&["send", &pane.pane_id, "one\ntwo"]);
assert!(sent.status.success(), "{sent:?}");
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Working);
rig.wait_for_row(&pane, |r| r.status == SessionStatus::Idle);
rig.paste(&pane, "/clear");
rig.wait_for_absent(&row.id);
let next = rig.wait_for_row(&pane, |r| r.id != row.id);
rig.wait_ready(&pane, Some(next.id.as_str()));
rig.close();
```

- `launch(config)` starts a named Python process in a new private window, with a
  shell retained after exit. `StandInPane` has `pane_id`, `pid`, `launcher`, `log`.
- `wait_ready(pane, optional_id)` requires the latest idle render, matching screen
  and tmux bracketed-paste flag; use it instead of sleeps. Row/readiness polls run
  every 20ms with 10-second deadlines. Row state can intentionally lag UI state.
- `wait_for_row(pane, predicate)`, `wait_for_absent(id)`, `pane_rows(pane)`, `rows()`
  use the real daemon. **SessionView has no PID**: use owned pane handles and IDs,
  never total host session counts. Discovery can see unrelated procfs processes.
- `paste(pane, text)` uses a tmux buffer with bracketed paste and separate Enter.
  `key(pane, "Escape")` sends a key only to the handle's private server. These
  controls are for fixture panes, not other agents' panes.
- `atm(args)` and `tmux(args)` return bounded command output, including expected
  failures. `command(path)` supplies the same sealed environment to custom CLI
  invocations; run it with `rig.run(&mut command)`. `tmux` always supplies `-L`.
  If also using the existing role-test helper, import `stand_in::bounded` rather
  than compiling `support/role.rs` a second time.
- `journal(pane)` supplies diagnostic JSONL (identity, accepted/rejected input,
  event/dropped, render, ready, selection). Event entries are **attempts**, not
  delivery acknowledgments: script exit zero never proves daemon receipt.
- `discover()` exercises actual process discovery. A pending-PID row upgrading to
  a single real-ID row proves that discovery and the real hook agree on identity.
- `close()` explicitly checks cleanup success. Drop is the fallback on unwind.
  Daemon child ownership and tmux socket/PID are rechecked before signaling; all
  commands clear ambient environment, including TMUX, TMUX_PANE and spawn overrides.

### Testing `atm spawn` or daemon-managed spawn

`rig.launcher(mode)` returns an absolute executable path for `ATM_SPAWN_BIN`
(or the appropriate per-harness override). It is a symlink named `claude`, `codex`
or `pi`, not a shipped Rust binary. The program accepts one positional initial
prompt, including a leading `-`. Current `atm spawn --prompt` rejects control
characters, so use quoted single-line prompts there and multiline `atm send`.

Set these in the **private tmux server environment** before spawning (not just the
CLI's environment: tmux's pane shell inherits its server):

```rust,ignore
let log = rig.root().join("spawn.jsonl");
for (key, value) in [
    ("ATM_STANDIN_LOG", log.to_str().unwrap()),
    ("ATM_STANDIN_ID_PREFIX", "my-spawn"),
    ("ATM_STANDIN_WORK_MS", "1200"),
] {
    assert!(rig.tmux(&["set-environment", "-g", key, value]).status.success());
}
let mut cmd = rig.command(std::path::Path::new(env!("CARGO_BIN_EXE_atm")));
cmd.args(["spawn", "--target-pane", "%0", "--prompt", "one quoted prompt;"])
   .env("TMUX", rig.tmux_env()) // only this private server, for CLI's in-tmux guard
   .env("ATM_SPAWN_BIN", rig.launcher(StandInMode::Claude));
let out = rig.run(&mut cmd);
assert!(out.status.success(), "{out:?}");
let pane_id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
let pane = rig.observe_pane(pane_id, StandInMode::Claude, log);
// observe_pane waits for a live identity, not idle: initial prompt may be working.
```

For respawn in the same pane, the journal may be reused: `observe_pane` selects
its newest live identity and verifies procfs start time, not just PID. Give each
new process a fresh ID prefix and wait for that process's expected session ID;
`wait_ready(None)` alone cannot distinguish an old ready screen from a new one.

The rig also inherits private ATM_SOCKET, ATM_TMUX_SOCKET, TMUX_TMPDIR,
ATM_STATE_DIR, HOME and XDG directories into its server. Direct program launch
requires `ATM_STANDIN_ROOT` and socket/log paths resolving inside it, a private
TMUX/PANE, and `ATM_STANDIN_ID_PREFIX`. Never unset ATM_SOCKET to use a default.

## Controls and fidelity

| Mode | Startup | Prompt / completion | Clear | Escape |
|---|---|---|---|---|
| Claude | SessionStart | UserPromptSubmit / Stop | `/clear`: SessionEnd, gap, fresh-ID SessionStart(source=clear) | no hook; scheduled Stop cancelled |
| Codex | **no event** | first prompt of each generation sends SessionStart then UserPromptSubmit; Stop completes | `/new`: fresh editor/ID internally, **no event until next prompt** | no hook; scheduled Stop cancelled |
| pi replay | session_start | input + agent_start / turn_end + agent_end | `/new`: session_shutdown, gap, new session_start | turn_end then aborted agent_end (idle) |

No event does not mean no row: Codex discovery can create `pending-{pid}`, and
its old session row remains after clear. **Waiting for a Codex new-ID hook before
sending the next prompt deadlocks.** Wait for the fresh editor (`wait_ready` with
the next deterministic ID, or journal/screen evidence), then submit. Pi's turn_end
is a usage update; agent_end is the lifecycle acknowledgment ending work.

- `/atm/dialog` is **Claude-only**: sends PreToolUse and PermissionRequest, shows
  captured question/Yes/No/Esc screen. Both daemon status refusal and `peek --prompt`
  are meaningful. Escape erases the dialog but leaves attention_needed in atmd.
  Enter records a selection, not a prompt, and emits no hook (synthetic safe
  selection behavior, not an emulation of tool approval). No command executes.
  **Codex has no dialog yet; pi replay models no native dialog** and does not run
  pi-atm or its extension UI/transport code.
- `/atm/drop-next [EVENT]` suppresses the next matching event, or literally the
  next event if no name is supplied. `/atm/drop-next Stop` preserves submission
  but loses completion. Names are case-sensitive; invalid names do not arm loss.
- `/atm/exit` or native `/exit` (Claude), `/quit` (Codex/pi) emits end and returns
  to the pane shell. Codex emits ends for all its generations. Ctrl-C also exits
  cleanly. Dropping the exit hook lets tests observe actual PID-based reaping.
- Ctrl-U clears the input draft. Escape interrupts a turn without exiting. No
  background completion survives an interruption, clear or dialog.
- Controls match whole submitted input; multiline text containing a control is
  ordinary prompt text. Busy/clearing input is rejected locally, not queued.
- `ATM_STANDIN_WORK_MS` defaults to 500, `ATM_STANDIN_CLEAR_GAP_MS` to 100. Negative
  values fail. IDs are `<prefix>-<generation>` starting at 1; `launch` qualifies
  config.id_prefix with the run and pane serial to prevent accidental collisions.
- Bracketed paste is enabled before readiness; UTF-8/delimiters may be fragmented.
  Like normal tty input, CR becomes LF (tmux paste-buffer translates LF to CR).
  A lone ESC resolves after a 20ms input gap; this is not a full terminal emulator.
- Claude/Codex payloads go through the real repository scripts and real socat.
  Pi uses a bounded persistent socket per session, but **only replays envelopes**;
  it does not test pi-atm reconnection, usage accounting or SDK behavior.
- No tokens, models, trust/first-run flows, subagents or real tool executions are
  simulated. Deterministic timers are not claims about real provider latency.

## Captured templates

Captured: Claude Code 2.1.325, 2026-11-14, U5 real TUI via timestamped private
Unix proxy into atmd. `claude_2.1.325.jsonl` selects source envelope lines
1, 3, 4, 6, 7, 14, 15, 18, respectively. The existing adapter's
`claude_tui_session_2.1.310.jsonl` independently confirms these lifecycle shapes.

Captured: Codex 0.167.0, 2026-11-14, U5 real TUI with `--no-daemon` via the
same proxy. `codex_0.167.0.jsonl` selects lines 3, 4, 5, 12.

Captured: pi 1.5.0, 2026-11-14, U5 real TUI with pi-atm via the same proxy.
`pi_1.5.0.jsonl` selects lines 1, 2, 3, 5, 6, 7, 8, 18, 19. The agent_end
messages are excerpts retaining only the assistant; system/tools/user context
and signed text metadata are omitted. This mode **replays captured envelopes;
it does not run pi or pi-atm**.

Sanitization: original paths become `/fixture/...`; session/prompt/turn/tool/
response identifiers become `fixture-*`; timestamps become zero. Forwarder-
added PID and pane fields are removed: runtime must derive them. Model names,
reason/source/stopReason fields and usage shapes are retained. The permission
screen is an excerpt of U5's Claude permission capture with marker text replaced.

Synthetic: runtime prompt, ID, path and timing substitutions construct controlled
scenarios from these templates. They are not new real-harness captures. No native
Codex or pi dialog was captured by U5, so neither mode claims dialog fidelity.
