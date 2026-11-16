# Promotion tools (Linux, Python 3.9+)

## Protected-file report

Run the report from the checkout before the merge: a unit can edit the report script itself.
From that repository root:

```sh
python3 scripts/atm-protected-files.py                 # origin/main to HEAD
python3 scripts/atm-protected-files.py BEFORE AFTER    # exact merge endpoints
```

The report prints both resolved commit hashes, status and JSON-escaped path for
each protected change, and a count. Both changed and clean reports exit zero;
invalid revisions/rules or Git errors exit nonzero. No fetching, hooks or write
protection. It reports **net tree differences**, not intermediate reverted edits,
and never substitutes the merge base. Renames are D+A, including moves out of
protected directories. Mode changes and binary changes are included.

`scripts/protected-files.txt` holds repository-relative `fnmatchcase` patterns;
`*` spans `/`. Blank/comment lines are ignored. Rules are the union of the shipped
list and lists in both revisions, so narrowing/deleting the new list cannot hide
changes covered by the old one. A missing historical list is allowed. The tools,
list and their tests protect themselves through `scripts/*`.

Owner policy is included: `.atm/intent.md`, `.atm/rules.md`, and all nested
`.atm/measures/*` rubric files, together with the intent parser, daemon policy
and rule-publication modules, admission/resolution hooks, and their tests.
Keep reviewed rubric files below `.atm/measures/`; `task.create` checks the
owner's listed measure on each call. Mechanical judge scripts belong under
already-protected `scripts/*`. The reporter does not execute commands, parse
intent, or discover arbitrary command dependencies.

Review this report at each integration merge and again at promotion. In
particular, treat an intent/rule/rubric deletion or a narrowed manifest as a
judge change, not ordinary housekeeping. Only committed net endpoint changes
are reported; untracked/local policy edits are not covered by a Git tree diff.
This remains detection, not write protection or authorization to change the
owner's intent. Project intent/rubrics are owner-approved inputs; setup does
not generate them. An owner's accepted `atm decide --rule` appends one answer
to the requester's project rules file; its file/event-log failure contract is
in the README's standing-rules section.

## Install and rollback

These scripts are **owner-run promotion tools**, not a release downloader or a
candidate lifecycle command. They require Linux procfs and Python 3.9+. Keep
`atm_promotion.py` beside the two entrypoints. No dependencies beyond stdlib.

```sh
scripts/atm-promote --from-bin /absolute/candidate/bin \
  --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
scripts/atm-rollback --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
```

Both `--bin-dir` and `--socket` are mandatory: there is no implicit live endpoint
or destination. In particular, a cargo install's bin directory is not the release
installer's default directory. The candidate kit's immutable
`installs/<sha>-<profile>/bin` directory is a suitable source; these scripts never
edit the kit, select/stop a candidate, build, fetch, start a daemon or run a model
CLI. Choosing the source is the owner's responsibility, not proof of provenance.

### Owner procedure

1. Require a green gate on the integration head, approved real-session smoke,
   and the owner's acceptance of the protected-file report. This unit does not
   supply that smoke or grant approval.
2. Choose a quiet moment, quit every TUI/sidebar, then **stop the daemon** using
   your normal verified procedure. An open TUI can autostart it. Confirm the bin
   directory and daemon endpoint before invoking either tool.
3. Run `atm-promote`. Only after success, start the daemon and restart sidebars.
   Expect the documented one-time loss of old in-memory cost/first-prompt data.
4. Check `atm list`, `atm status`, and `atm send` to a scratch pane. If any check
   fails, quit TUIs and stop the daemon **again**, then run `atm-rollback`.
   Restart only after rollback/setup succeeds; otherwise investigate the output.

### Storage and failure contract

Installed `atm` and `atmd` remain regular executable files. Promotion takes a
nonblocking lock at `<bin>/.atm-promotion/lock`, copies both current binaries
(with modes) to `pairs/pair-<unique>/`, and atomically records that directory in
`previous.json`. It stages both new files inside the destination, then renames
**each** into place. No managed entrypoint symlinks; a later cargo install can
still replace the ordinary files. The previous record refers to the most recent
pre-promotion pair, not necessarily the build cargo most recently installed.
Promotion refuses an already-installed pair (same bytes and modes), preserving
its rollback target.

The rename pair is deliberately **not atomic**. SIGHUP, SIGTERM, SIGKILL or power loss between
renames can leave mixed binaries. With the daemon stopped and TUIs closed,
nothing should execute them until the owner runs rollback, which restores both.
A leftover `.atm-stage-*` file blocks further promotion: run rollback first.
Rollback removes those files after restoring both binaries. Do not remove them
and rerun promotion on a mixed pair: that would save the mixture as previous.
No power-loss durability or unattended recovery is promised.

After replacement, the absolute destination `atm setup` runs with a 60-second
hang guard, closed stdin, destination-first PATH, explicit ATM_SOCKET and no
TMUX/TMUX_PANE. HOME and XDG directories are deliberately the invoking owner's:
setup refreshes hooks, both skills, pi package/registration, tmux bindings and ATM
config using the installed binary's embedded assets. The hook scripts still go
under `$HOME/.local/bin`, independently of `--bin-dir`. Existing harness home
directories determine which integrations setup refreshes. No detected harnesses
means setup has nothing to refresh. Setup does not source bindings into tmux;
Codex hook trust and restarting/reloading an already-loaded pi extension remain
owner actions.

Setup output is printed. Nonzero exit, timeout, or `failed:` anywhere in its
output is failure: verified with atm 0.3.0, skill installation can print that
word while returning zero. This conservative diagnostic guard is not a stable
machine-readable setup API; recheck it when promoting a version with changed
setup diagnostics. Invalid output bytes are replaced for display, not allowed
to bypass recovery. On install/copy/rename/setup failure after the backup was
recorded, both saved files are copied/renamed back and old setup reruns. The
command still exits nonzero and reports recovery success or failure. Recovery
also runs for unexpected exceptions and KeyboardInterrupt (Ctrl-C). Failure
before the complete backup is recorded leaves the installed pair and prior
record alone. Successful recovery restores the prior previous record if one
existed; otherwise the first backup remains selected. If recovery fails, the new
backup stays selected for rollback; do not restart.

Rollback copies/renames the recorded previous pair into place and reruns its
setup. It retains that same previous record, so repeated rollback is idempotent,
not a toggle. A setup failure during rollback leaves the restored old pair in
place and reports failure. Before replacing a complete, differing installed pair,
rollback archives it without changing the previous record. Repeated rollback of
the same bytes/modes creates no extra archive. Generation directories are retained
without pruning; missing installed entries can still be repaired from previous. Asset
writes are not a transaction: rerunning old setup repairs its managed assets,
not arbitrary configuration changes. Persistent daemon state is never moved or
deleted; compatibility of an actual older binary with newer state must be
verified separately.

### Refusals and limits

- Both source executables and both installed executables must be present,
  regular, caller-owned and executable for promotion. Rollback requires a
  complete saved pair; it can repair missing installed entries.
- Source and destination must be absolute, nonoverlapping, nonsymlinked,
  caller-owned directories, without group/other write access. Metadata and
  backup directories are private. Symlinked executables/metadata, invalid
  previous records, missing previous pairs, and a held lock are refused.
- Any existing explicit socket path is refused, even a stale file/symlink.
  The script never connects to it or removes it. Investigate stale paths rather
  than bypassing this check.
- Procfs preflight refuses running installed/saved binaries (including deleted
  executable paths), and other readable same-user process environments naming
  the selected socket. Calling shells/ancestors are exempt from the environment
  check, but **not** from the executable check. No daemon/process is signaled by
  preflight; timeout can terminate only the setup subprocess it started.
- Procfs may hide exe/environ for unrelated same-user services (observed with
  the user systemd manager). In that case readable comm/argv identify possible
  ATM processes: those, or unreadable/empty identity, fail closed. Clearly
  unrelated inaccessible services are ignored. Disguised processes, other UIDs,
  privileged/hostile writers and concurrent autostarts are outside this
  best-effort check. The lock coordinates these tools only; it cannot prevent
  cargo install or a TUI from racing. Owner quiescence is mandatory.

## Rollout in one command

`atm-rollout` is the owner procedure above as one command. It calls `atm-promote`
and `atm-rollback` unchanged and adds what was done by hand around them. Nothing
runs unattended: you start it, and the protected-file report is still yours to accept.

```sh
scripts/atm-rollout /absolute/candidate/install --bin-dir /absolute/installed/bin \
  --socket /absolute/daemon.sock [--installed-commit REV] [--report-sha256 HEX] \
  [--wait[=SECONDS]] [--real HARNESS] [--dry-run]
scripts/atm-rollout --rollback --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
```

The candidate is a candidate-kit install directory: `install.json` (its `sha` and
`hashes`) beside `bin/`. Run it from the root of the checkout at the candidate's
commit, in a shell like the one the daemon was started from: the report uses Git
in the current directory, and the daemon is restarted with this shell's
environment. `--bin-dir` and `--socket` are mandatory, as above. Every `atm` and
`atmd` call gets `ATM_SOCKET` set to `--socket` and no `TMUX_PANE`, so its calls
count as the owner's.

| Step | What runs | Refuses with (exit 1) |
|---|---|---|
| 0 | Path and pair checks (those of `atm-promote`), `install.json` hashes, a connect to the socket, `atmd status`, the daemon's executable and start environment | `path must be absolute and contain no symlinks or ..` · `not an owned, safely permissioned directory` · `not an owned regular file` · `not executable` · `missing tool: tmux` · `interrupted promotion; run atm-rollback first` · `candidate and destination must not overlap` · `install.json names no commit and hashes` · `candidate binaries do not match install.json` · `candidate pair already installed` · `no daemon answers` · `atmd status names no running daemon` · `the daemon (pid N) does not run <bin>/atmd` · `pid N also runs <bin>/atmd; stop it first` · `the daemon was started with KEY=...` (`HOME`, state, config or tmux variables differ from this shell) · with `--rollback`: `the previous pair is already installed`, and `atm-rollback`'s refusals of a missing or invalid previous record |
| 1 | `atm-protected-files.py <installed commit> <candidate commit>`, printed with its sha256. Accept it by typing `yes` on a terminal, or with `--report-sha256` of the report you already read. The installed commit is the one the last rollout recorded in `<bin>/.atm-promotion/rollout.json`, else `--installed-commit` | `installed build's commit is unknown` · `--installed-commit disagrees with the recorded` · `protected-file report failed` · `protected-file report not accepted` (any answer but `yes`, Ctrl-D, a wrong hash) · `the report needs your yes` |
| 2 | Smoke of the candidate pair on a private socket, state directory and tmux server in a temp directory: `agent up`, `msg send` until delivered, `agent clear`, `agent down --kill`, on the repository's pi stand-in (`tests/support/stand_in`), or on a real harness with `--real` | `smoke failed at <start, up, message, clear, down>: ...; run kept in <dir>` |
| 3 | The state before: `atm list`, `atm agent list`, each agent's unread inbox, Claude's `statusLine`, every process on the installed pair. Sessions mid-turn are listed, not refused. An agent whose recorded pane no longer exists gets a warning (`agent X's pane %P is gone; the restart will record it down`), not a refusal. An `atm` command that is not a sidebar refuses only if it is still running five seconds later: a loop's short `atm list` is waited out | `agent X is starting` · `agent X is clearing` · `message M to X is being delivered` · `pid N runs "atm ...", not a sidebar` · `sidebar pid N is not in a tmux pane` · `sidebar pid N was not started from the shell of pane %P` |
| 4 | Each sidebar gets `C-c` in its pane and is waited for. The rollout waits for the daemon's lock (`<socket>.lock`), runs `atmd stop`, and holds the lock | `sidebar pid N in %P did not quit` · `the daemon did not stop` · `another daemon took the socket` (each after the restore described below) |
| 5 | `atm-promote`, in its own session | Its own refusal or failure, when the previous pair is still in place (after the restore below; a line says so if `atm setup` ran for both builds). If `atm-promote` says its own recovery failed ("do not restart yet"), nothing is started: **exit 4** |
| 6 | The lock is released; `atmd start -d`; wait for the socket to answer | |
| 7 | Checks: `identity` (the installed bytes and modes are the candidate's, and the answering daemon runs the installed file), `list`, `status`, `agent-list` (asked again for up to a minute while the daemon says it is still starting), `agents` (every named agent of step 3 is there, and is on the same pane unless it is now `down`; both are skipped, in words, for a build whose `atm` has no `agent` command), `send` (one `atm send` into a detached scratch tmux session, closed again) | |
| 8 | On a failed start or check, or a Ctrl-C during `atm-promote` (check `interrupted`): stop, `atm-rollback`, start, the same checks against the previous pair. **Exit 3** names the failed check | |
| 9 | Each sidebar's command line is typed back into its pane. What differs from step 3 is printed: sessions gone or new, agents whose state changed (`-> down` and `-> stuck` first), a `statusLine` that `atm setup` replaced (with the command that puts yours back; nothing is restored for you). An agent of step 3 that is now `down` while its pane still exists gets a `rollout: WARNING: agent X is now down but its pane %P is still there` line just before the last line, here, after a rollback (exit 3) and after a refusal that follows a restart; after a rollout it ends with the `--rollback` command. The installed commit is recorded | |

**A refusal says how far the run got.** Before anything is stopped (steps 0 to 3) the
text is `rollout: refused, nothing changed: <reason>`. From step 4 on, the rollout
puts back what it stopped: it releases the lock, starts the daemon if none answers,
types the sidebars back, runs the checks that only read (`identity`, `list`,
`status`, `agent-list`, `agents`) against the pair that was installed, and prints
what differs. The text is then `rollout: refused before the swap; the installed build
is unchanged and was started again (daemon pid A -> B): <reason>` (or `kept running
(daemon pid A)` when only sidebars were quit), still exit 1. A restart is a change:
sessions that report on their next event lose their rows, in-memory data is gone,
and the start may record agents stuck or down. If one of those checks fails, or a
sidebar is not back, the run ends in exit 4.

| Exit | Meaning |
|---|---|
| 0 | The new build (with `--rollback`, the previous one) runs and passed the checks |
| 1 | Refused. Either nothing changed, or the installed build is unchanged and was started again: the first line says which |
| 2 | Usage |
| 3 | Rolled back: the previous build runs and passed the checks; the failed check is named |
| 4 | `NEEDS YOU`: with which pair is installed, whether a daemon answers, and the sidebars that are not back. The `statusLine` note is printed here too once `atm setup` has run |

`--dry-run` runs steps 0, 1 (no question asked) and 3, which only read, and prints
the commands of the rest. `--wait` polls step 3 until nothing is in flight (300
seconds unless given) instead of refusing. `--rollback` is steps 0, 3, 4,
`atm-rollback`, 6, 7 and 9 for the day the previous pair should come back; a failed
check then exits 4, it does not swap again.

**After a rollback, exit 4 may name the organisation log.** Only when the previous
daemon answers `agent list` with "organisation service unavailable: events replay
..." does the rollout say that it cannot read the log the newer build wrote. Any
other failure of that check is reported as itself.

### Why the daemon's lock

`atm list`, `atm send`, every RPC command and an open TUI start the daemon when none
answers. Between a stop and a swap that brought the old daemon back. `atmd` takes
`flock` on `<socket>.lock` before it binds and holds it until it exits; the rollout
waits on that lock before the stop, so it is first in line when the daemon lets go,
and keeps it until the swap is done. A caller in between gets `Daemon failed to
start within 3 seconds` instead of an old daemon. A start that asks in the very
instant the lock is free can still win it; the rollout then refuses, leaves that
daemon running and types the sidebars back. Verified on a private socket with
a stage 0 release pair; 0.3.0 has the same lock.

### Limits

- **Where and how to run it.** In a tmux pane of the server the agents and sidebars
  live on, and leave that pane open until the last line: a closed terminal ends the
  rollout at once, with no message, a daemon that may be stopped and sidebars that
  are not back. **Ctrl-C is safe while `$ atm-promote ...` or `$ atm-rollback ...`
  runs:** the tool has its own session and is waited for, and after `atm-promote`
  the Ctrl-C means "roll back". At another moment after the stop, Ctrl-C ends in
  exit 4 with the state line. **No Ctrl-Z:** a stopped rollout keeps the lock, and
  every `atm` caller fails until it is resumed.
- A restart leaves an agent that was starting or clearing `stuck`, and a message
  that was being delivered held until `atm msg retry`. Step 3 refuses those; work
  that begins between step 3 and the stop is not seen.
- Agent states are compared in words only (step 9): a state the new daemon recorded
  is in the log, and a rollback would not undo it. A `down` agent passes the `agents`
  check whether its pane is gone or alive, because a harness that exited and a build
  that misreads live panes at its start leave the same listing: such a build ends in
  exit 0, and the `WARNING` line is the only sign of it. Sessions are compared by pane: an
  idle session of a harness that only reports on its next event shows as "only a
  placeholder row until its next event".
- **Rollback onto newer state.** An older daemon skips event kinds it does not know.
  A log it cannot replay makes it start with the organisation methods disabled: the
  rollout then exits 4 and says that the previous build runs but named agents and
  messages are unavailable until the newer build is back. The state directory is
  never moved, edited or deleted. Run on a private copy with real release pairs:
  0.3.0, which has no named agents at all, started and answered on a state
  directory a stage 0 build had written its organisation log and messages to, left
  that log byte for byte as it was, and the stage 0 build read it back afterwards.
- The sidebar restart types into the pane's shell, as `atm workspace` does, once that
  shell is in the foreground of its terminal again. It does not look at what was
  typed there since the quit. A sidebar that the tmux
  new-window hook starts while the daemon is down fails and is not looked for.
- The scratch session is created on the tmux server `atm` itself uses and killed by
  its id. What a user's own tmux hooks do with a new detached session is not known.
- `--real <harness>` starts model turns on the owner's account and has only run
  against the stand-in placed under the harness's name: not verified with a real CLI.
- The lock stops daemons on this socket. It does not stop `cargo install` or a
  daemon on another socket; `atm-promote`'s own checks still apply.

## Verification

`cargo test --test candidate_kit` and `cargo test --workspace` run the candidate
kit suite, `promotion_kit_test.py` and `rollout_kit_test.py` through the Cargo bridge. For
local combined Python discovery after building both binaries:

```sh
CANDIDATE_TEST_ATM="$PWD/target/debug/atm" \
CANDIDATE_TEST_ATMD="$PWD/target/debug/atmd" \
  python3 -B -m unittest discover -s scripts -p '*_test.py' -v
```

Use the repository's sealed test environment for these commands: private HOME,
XDG, ATM_STATE_DIR, ATM_SOCKET, TMPDIR and TMUX_TMPDIR; no inherited TMUX or
TMUX_PANE, no user-installed binaries on PATH. Tests use fake home/bin directories,
not the owner's assets. They exercise a real private daemon refusal and real
setup against Cargo-built pairs, never a model harness. The rollout tests run whole
rollouts on a private daemon and a private tmux server with a real sidebar, and
use the stand-in pi where an agent is needed. Same-version pairs prove
promotion/rollback mechanics, not a real historical downgrade or state recovery.
