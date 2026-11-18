# Registry input sidecars — SEALED REPLAY

These seven versioned tapes pair accepted registry commands with environment
answers and complete per-turn golden checkpoints. They are **not** records of
the historical harness's clock, process table, git state or acceptance order.

## Original envelopes (unchanged)

- Captured: Claude Code 2.1.310, 2026-10-24, sealed TUI in private tmux with a
  fake Messages API; hook stdin and status-line stdin wrappers. The atmd fixture
  `claude_tui_interleaved_2.1.310.jsonl` orders those captures; a test checks its
  payload multiset against both unchanged adapter fixtures.
- Captured: Claude Code 2.1.325, 2026-11-16, sealed stub-mode rig, private
  HOME/config/project, fake Messages API, no external route. The adapter's
  `claude_turn_boundaries_2.1.325.jsonl` covers normal/blocked/rewake/compact.
- Captured: pi 0.73.1, 2026-10-24, sealed local fake-provider setup, pi-atm wire
  frames. The adapter's tool-run, new-session and session-cost fixtures are
  replayed separately in their original frame order.
- Captured: pi 1.5.2, 2026-11-16, sealed interactive CLI/private tmux and real
  atmd recording proxy. The adapter's reconnect fixture retains its original
  case/frame order.
- Captured: codex-cli 0.163.0, 2026-10-24, sealed TUI/private tmux, fake local
  provider, hook-stdin wrappers. The adapter's TUI hooks and unchanged rollout
  token records supply the lifecycle and transcript-derived usage respectively.

The adapter crate docs are authoritative for each original method and quirk.
No current harness or model CLI is run to make these sidecars.

## Added sidecar provenance

Captured: atmd 0.4.2 (private registry input seams), 2026-11-17, **SEALED
REPLAY** through the real in-process ConnectionHandler and registry dispatcher,
using in-memory connection I/O, an owned stdin-gated `cat` process, private git
repository and copied Codex rollout. Raw logging is enabled and flushed at each
frame; every raw record is counted and drops must be zero. Registry publication
queues are drained at each command/result barrier; lag fails the capture.

Synthetic: PID/pane/cwd/transcript attribution and initial discovery use the
owned stand-in, not the original harness processes. Source envelope order is
preserved; original offsets are not sleeps or claims about current timing.
Explicit git-result turns gate detached workers by channel, independent of
worker arrival order. Added alias routing, replacement/late-status, queries,
refresh, exited-process cleanup and supplied Register/Remove continuations are
synthetic schedules, not additional harness captures.

Only path/PID identity is scrubbed: owned root and shortened cwd view become
`$SEALED`, original generic home becomes `$CAPTURE_HOME`, and the owned PID
becomes `900001` consistently in commands, indexes, tombstones and process-query
keys. Clock samples, process starttimes, monotonic nanosecond offsets, measured
usage and domain values are otherwise retained exactly. Unordered maps/alias
and tombstone containers are canonicalized; domain vectors are not sorted.

Each `turns[]` entry contains `action` (the existing command's fields or a git
completion), `inputs` (ordered UTC/instant samples and process answers keyed by
query/PID/occurrence), `comparison` (view time) and `checkpoint` (golden domain,
views, both indexes, infrastructure, accepted start, aliases/links, tombstones
and in-flight git metadata). Missing, mismatched or unused inputs fail closed.
Codex derived ContextUpdate commands carry token counts; registry replay never
opens a historical transcript or falls back to the host's proc/git/clock.
Legacy core constructors still physically sample UTC; those discarded values
are replaced before visibility, as the timestamp tests prove.

Two fresh replays compare every checkpoint, including nonempty pre-end states;
prefix plus continuation equals whole replay. A separate continuation test
shows that identical visible rows without aliases/tombstones misroute later
commands. Literal Codex token expectations guard against empty usage replay.

To regenerate, run the named test with `ATM_RECORD_REGISTRY_REPLAY=1` under the
repository's fully sealed, direct-toolchain test environment (main disk,
private HOME/state/socket/tmux directories, no installed atmd on PATH):

```sh
nice -n 10 "$TC/cargo" test -p atmd --lib \
  captured_registry_replays_match_every_checkpoint_twice_and_continuation
```

Ordinary test runs only read checked-in tapes. A decoy-repository subprocess
test proves the capture's git commands ignore inherited GIT_DIR/GIT_INDEX_FILE.

## Deliberate limit

Production raw logs currently record only the four incoming envelope kinds,
not complete discovery/liveness/clock/git/acceptance inputs; they can also drop
frames. These sidecars do not make arbitrary historical raw logs replayable.
Live-log recording completeness needs a separate approved unit. No production
raw schema, writer, startup or wire contract changes here.
