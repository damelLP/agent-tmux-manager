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
Required hook/skill/extension assets are checked at the ref **before** building;
older refs missing any of them are refused with the missing source path.
`--debug` selects debug instead. One kit-owned target directory is reused under
the kit lock, never a repository worktree's target. The real toolchain Cargo is
used with installed user binaries excluded from build PATH. Git ref/asset and
worktree commands, plus build children, receive no inherited `GIT_*` variables:
hook-provided repository/index overrides cannot redirect them to another repo.
The detached source worktree is removed afterward. Build commands have a 30-minute hang guard.
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

Every kit tmux/daemon invocation strips inherited `TMUX` and `TMUX_PANE`.
The atm wrapper preserves pane context only when the ownership token and TMUX
socket/server PID match the verified candidate; foreign context is stripped. Candidate routing
is seeded **before** the private tmux server reads the user's configuration:
`ATM_SOCKET`, `ATM_STATE_DIR`, `ATM_TMUX_SOCKET=candidate`, private `TMUX_TMPDIR`,
and wrapper-first `PATH`, then the candidate binaries. `atm` in candidate panes
resolves to `<run>/bin/atm`, which delegates to this kit's refusal wrapper (keep
the kit scripts available while running). Every kit tmux command explicitly selects that
server. The environment also contains an ATM run-ownership token. Attached
clients cannot import routing/PATH through tmux's `update-environment`.

Use `attach` or the `atm` wrapper, not an unsealed external tmux client:
**tmux takes PATH from a new-window client's environment**, even when global
PATH is correct. Shell startup files can also reorder PATH; check `command -v
atm` in a fresh candidate shell if your shell customizes it. User tmux config,
plugins and bindings load normally; no plugin installation/update is performed.
This is process routing, not a security sandbox for arbitrary user config.

The kit never changes HOME, XDG state, hooks, skills or pi extensions, and never
runs `atm setup`. Wrapper calls containing `setup`, `uninstall` or `workspace`
are refused, including ordinary `atm` commands inside candidate panes. These
commands write shared live assets: workspace sidebar scripts still live under
`~/.local/share/atm`, and configuration under `~/.config/atm`. Direct calls to
`<install>/bin/atm` bypass the guard: **do not run these three commands through
a direct binary path**, or reorder PATH to bypass the wrapper. The kit is not
a filesystem sandbox; shared notification configuration also remains shared.
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
Like `atmd stop`, each pinned process gets up to 75 seconds from signaling to exit,
with one progress line on stderr after 5 seconds. This also applies when waiting
without signaling after an ownership change, and to the private tmux server's
exit. The bound is per process, not a total kit-stop deadline; processes are stopped
in sequence. A nominal daemon shutdown can take 66 seconds, but blocked filesystem or
Tokio blocking-pool work can last indefinitely. A timeout names the PID that may
still be shutting down and retains the runtime/manifest; it does not force exit
or claim that every pending write was persisted.
PID plus procfs starttime identifies a process; a reused/dead recorded PID is
gone, not foreign. Deleted/replaced executables remain visible during cleanup.
After a reboot removes the runtime directory, status/stop accept the stale
manifest only when the ownership-token scan finds no processes. Status stays
read-only; stop removes the manifest so another candidate can start.
If ownership changed or a process refuses to exit, it fails closed and retains
the manifest/runtime for diagnosis; no broad name-based kill or SIGKILL fallback.
Run stop from outside the candidate server; inside-server stop is refused
before it can hang up its own terminal. Unverified: processes deliberately
stripping all routing/ownership environment or escaping through privilege
boundaries. These are not supported; this is not a cgroup/container supervisor.

The initial M1 trial policy in this section allowed host-wide discovery and
said not to filter foreign **pending rows without panes**. Named-server
isolation supersedes that policy: U11b's concurrent stand-in runs proved that
host processes fill a private daemon's 100-row registry and reject its own
sessions; failure diagnostics also exposed host sessions and working directories.

With a nonempty `ATM_TMUX_SOCKET`, discovery now admits only processes mapped
to that server's panes. The candidate sets this label, so startup and explicit
rescans exclude live sessions outside its server. An unavailable, empty or
failed pane listing admits no discovered processes, rather than falling back
to host-wide discovery. Direct hooks/events are still accepted; this is not
an authentication boundary, and historical logs are not purged.
The default daemon, and a daemon that only relocates `ATM_SOCKET` (for example
a devcontainer setup), retain host-wide discovery. They may still list candidate
harnesses without panes, as can candidates installed from older refs. A foreign
row assigned a local pane remains a stop-the-trial defect.
The kit does not isolate harness user state or model credentials and does not
start any model CLI itself. At pre-U7 refs, `atm spawn` still requires an actual
tmux pane: run it inside the attached candidate, not through the outside wrapper.
Daemon-routed send/spawn are verified separately when that milestone lands.

`stop` retains state; `uninstall` removes only the named binary install unless
`--wipe-state` is supplied in the same call. Retained state and the build cache
are not garbage-collected automatically.

Known operational limits:
- `atm` passthroughs, read-only `status`/`installs`, and `attach` validation
  share the kit lock. Multiple commands can run together. Install/build, start,
  stop, restart-daemon and uninstall hold it exclusively for their whole operation
  (builds have a 30-minute guard). Contenders wait up to 30 seconds, then report
  a timeout with the holding operation/PID when Linux procfs exposes it.
- The wrapper retains its shared lock until `atm` exits, without leaking it to
  spawned daemons or harnesses. Exit long-running `atm` TUIs/watch commands before
  lifecycle operations; otherwise those operations can time out. `attach` releases
  its lock after validation so an attached terminal does not prevent `stop`.
  Shared validation can recover a server identity in memory but never writes it.
- Direct binary calls bypass the lock as well as the mutation guard: they can
  autostart a daemon during restart or stop. A dying harness can also briefly
  leave a hook process in the final token scan. An in-pane caller that starts
  during `stop` now waits for the kit lock and can make `stop` fail with
  "descendants remain"; re-run `stop` once that caller exits.
  The runtime/manifest is retained on failure; close direct candidate TUIs and
  re-run from outside. These race schedules are unverified; ordinary autostart
  cleanup has a real-process test.

## Checks

`bash -n scripts/atm-candidate`; Python syntax can be checked without writing
bytecode using `compile(path.read_text(), str(path), 'exec')`.
`cargo test --test candidate_kit` runs the real-process Python contracts with
Cargo's prebuilt binaries, private HOME/roots and real tmux; it never recursively
runs Cargo. Use the repository's sealed test environment for all Cargo tests.
