# Candidate kit (Linux)

Run from any directory using the path to `scripts/atm-candidate`. Keep its
`atm_candidate.py` helper beside it. Requires Bash, Python 3.9+ with Linux
pidfds, tmux, git and (for source installs) rustup/Cargo. No Python packages.

```sh
scripts/atm-candidate install integration/control-plane
scripts/atm-candidate installs
scripts/atm-candidate start                 # last installed ID
scripts/atm-candidate status                # JSON; never autostarts
scripts/atm-candidate atm list -f json
scripts/atm-candidate attach                # from a terminal OUTSIDE tmux
scripts/atm-candidate restart-daemon
scripts/atm-candidate stop
scripts/atm-candidate start <older-id>
scripts/atm-candidate stop
scripts/atm-candidate uninstall <id>         # retains persistent state
scripts/atm-candidate uninstall <id> --wipe-state
```

`install REF` resolves a full commit SHA, creates a detached worktree under the
kit build root, and runs `cargo build --locked --release --bin atm --bin atmd`.
`--debug` selects debug instead. One kit-owned target directory is reused under
the kit lock, never a repository worktree's target. The real toolchain Cargo is
used with installed user binaries excluded from build PATH. The detached source
worktree is removed afterward. Build commands have a 30-minute hang guard.
Installs are immutable SHA/profile-named binary pairs, published atomically;
reinstalling an existing ID is refused. The manifest records hashes/toolchain.

For tests or an independently built pair, `install REF --prebuilt BIN_DIR`
copies `BIN_DIR/atm` and `BIN_DIR/atmd`, without invoking Cargo. Its ID/profile
is distinct and its manifest explicitly marks the claimed ref provenance as
**unverified**. Normal trials should use source installs.

## Locations and routing

| Data | Default | Override |
|---|---|---|
| Installs/manifests/lock | `$HOME/.local/share/atm-candidate` | `ATM_CANDIDATE_ROOT` |
| Per-install state | `$HOME/.local/state/atm-candidate` | `ATM_CANDIDATE_STATE_ROOT` |
| Source staging/shared target | `$HOME/.cache/atm-candidate` | `ATM_CANDIDATE_BUILD_ROOT` |
| Parent of fresh short runtime directories | `/tmp` | `ATM_CANDIDATE_RUNTIME_ROOT` |

Overrides name **new**, absolute, nonsymlinked directories. The kit marks them
as owned; it refuses preexisting unmarked roots and roots accessible by other
users. Install/state/build roots must be separate. Runtime directories are
fresh mode-0700 `atmc-<uid>-*` directories; excessively long socket paths fail
closed. Keep runtime overrides short. Do not edit manifests.

Every kit tmux/daemon/atm invocation strips inherited `TMUX` and `TMUX_PANE`. Candidate routing
is seeded **before** the private tmux server reads the user's configuration:
`ATM_SOCKET`, `ATM_STATE_DIR`, `ATM_TMUX_SOCKET=candidate`, private `TMUX_TMPDIR`,
and candidate-first `PATH`. Every kit tmux command explicitly selects that
server. The environment also contains an ATM run-ownership token. Attached
clients cannot import routing/PATH through tmux's `update-environment`.

Use `attach` or the `atm` wrapper, not an unsealed external tmux client:
**tmux takes PATH from a new-window client's environment**, even when global
PATH is correct. Shell startup files can also reorder PATH; check `command -v
atm` in a fresh candidate shell if your shell customizes it. User tmux config,
plugins and bindings load normally; no plugin installation/update is performed.
This is process routing, not a security sandbox for arbitrary user config.

The kit never changes HOME, XDG state, hooks, skills or pi extensions, and never
runs `atm setup`. Wrapper calls containing `setup`/`uninstall` are refused.
`status` reports installed-asset byte drift against the named ref (hooks, both
skills, pi package/extension); missing/different assets are information, not
proof of wire incompatibility. Review drift at each milestone. Existing
installed hooks/extensions must honor `ATM_SOCKET`.

## Lifecycle and limits

One active candidate per kit root. Stop before switching. Daemon startup waits
for a real handshake/list round trip, not merely a socket. The daemon runs in
foreground mode detached from the launching terminal, with output appended to
`<state>/<id>/candidate.log`. Its PID, procfs starttime and executable, plus the
tmux server identity, are recorded. Restart preserves the exact state directory
and endpoint; it never uses an unverified `atmd stop` PID file. State is per
install because backwards event-log compatibility is not assumed.

Stop validates server socket/PID and process identity before signaling. Linux
pidfds prevent PID-reuse signals. It finds run-tagged descendants, including
sibling-atmd CLI autostarts, kills the private tmux server, stops owned processes
and removes the runtime only after exit. Failed startup follows the same cleanup.
If ownership changed or a process refuses to exit, it fails closed and retains
the manifest/runtime for diagnosis; no broad name-based kill or SIGKILL fallback.
Run stop from outside the candidate server. Unverified: processes deliberately
stripping all routing/ownership environment or escaping through privilege
boundaries. These are not supported; this is not a cgroup/container supervisor.

At the initial trial ref, procfs discovery is host-wide: another daemon can list
candidate harnesses as **pending rows without panes**, and vice versa. Do not
filter those rows. Foreign rows assigned a local pane are a stop-the-trial defect.
The kit does not isolate harness user state or model credentials and does not
start any model CLI itself. At pre-U7 refs, `atm spawn` still requires an actual
tmux pane: run it inside the attached candidate, not through the outside wrapper.
Daemon-routed send/spawn are verified separately when that milestone lands.

`stop` retains state; `uninstall` removes only the named binary install unless
`--wipe-state` is supplied in the same call. Retained state and the build cache
are not garbage-collected automatically.

## Checks

`bash -n scripts/atm-candidate`; Python syntax can be checked without writing
bytecode using `compile(path.read_text(), str(path), 'exec')`.
`cargo test --test candidate_kit` runs the real-process Python contracts with
Cargo's prebuilt binaries, private HOME/roots and real tmux; it never recursively
runs Cargo. Use the repository's sealed test environment for all Cargo tests.
