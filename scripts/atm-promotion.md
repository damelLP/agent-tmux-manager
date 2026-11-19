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
  ATM processes: those, or unreadable/empty identity, fail closed unless proven
  exited as described below. Clearly
  unrelated inaccessible services are ignored. Disguised processes, other UIDs,
  privileged/hostile writers and concurrent autostarts are outside this
  best-effort check. The lock coordinates these tools only; it cannot prevent
  cargo install or a TUI from racing. Owner quiescence is mandatory.

  Before refusing an inaccessible ambiguous identity, preflight rechecks procfs
  state: only an absent PID or a well-formed single-threaded zombie (`Z`, one
  thread) is treated as exited. Non-dumpable zombies can still deny exe/environ
  reads while exposing empty argv. A zombie leader with live workers is not
  exempt; live, malformed or unreadable state still refuses, including a live
  PID reused between inspection and the recheck.

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
| 3 | The state before: `atm list`, `atm agent list`, each agent's unread inbox, Claude's `statusLine`, every process on the installed pair. Sessions mid-turn and `starting` agents with a bound session (revalidation pending) are listed, not refused. An agent whose recorded pane no longer exists gets a warning (`agent X's pane %P is gone; the restart will record it down`), not a refusal. An `atm` command that is not a sidebar refuses only if it is still running five seconds later: a loop's short `atm list` is waited out | `agent X is starting` · `agent X is clearing` · `message M to X is being delivered` · `pid N runs "atm ...", not a sidebar` · `sidebar pid N is not in a tmux pane` · `sidebar pid N was not started from the shell of pane %P` |
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
- A restart interrupts a `starting` agent without a bound session or a `clearing`
  agent, leaving it `stuck`, and holds a message being delivered until `atm msg retry`.
  Step 3 refuses those; work that begins between step 3 and the stop is not seen.
  A bound `starting` agent is revalidated again instead.
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

## Supervisor

`atm-supervise` runs the rollout above, then observes one probation. Start it
explicitly and keep it running until its verdict; no daemon, TUI or hook starts
it. Keep `atm_supervisor.py` and the other kit files beside the entrypoint. Run
from the checkout containing the reviewed kit, with the candidate-kit roots
selected (see `atm-candidate.md`); the candidate build can be a different commit. Every command requires an explicit `--bin-dir`; probes use
raw socket requests, never an `atm` call that could autostart a daemon.

```sh
scripts/atm-supervise init --installed-commit FULL_SHA \
  --bin-dir /absolute/installed/bin [--install FULL_SHA-debug]
scripts/atm-supervise status --bin-dir /absolute/installed/bin
scripts/atm-supervise gate FULL_SHA-debug --gate-pass /absolute/gate-pass.txt \
  --gate-runner-label gate-runner --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
scripts/atm-supervise promote FULL_SHA-debug --approve REPORT_SHA256 \
  --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
scripts/atm-supervise check --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
```

The install argument is an immutable candidate-kit **ID**, not a path. `init`
records the existing installed pair once, matching its full commit, bytes and
modes to a retained install; use `--install` if matches are ambiguous. Its baseline
report is an observation, **not approval**. `status` prints the verified slots
without probing the daemon. All commands take the nonblocking
`<bin>/.atm-promotion/supervisor.lock`; a concurrent supervisor refuses and names
the holder. Rollout retains its separate daemon/swap locks.

### Slots and approval

`<bin>/.atm-promotion/slots.json` is checksummed and atomically replaced. It holds
`stable`, `previous`, `candidate`, the last three `rejected` records, and used
approval hashes. Each record binds commit, install ID, both binaries' hashes and
modes, install-manifest hash, retained kit and report hashes. Every read verifies
the retained artifacts and that the installed pair is stable or pending candidate;
drift is NEEDS YOU, not permission to repair files. Checksums detect drift, not a
hostile owner rewriting all evidence. Retained installs, kits, reports and evidence
are not automatically pruned.

Only acceptance after probation rotates slots: previous becomes stable, stable
becomes candidate, candidate clears. A gate pass or a swap alone rotates nothing.
Promotion stages candidate and consumes approval **before** invoking rollout;
a recorded failure rejects it without rotating stable/previous. A used hash is refused. A
rejected commit/pair is refused by gate and promote; the same report cannot retry
it. Do not clear rejection/approval records to retry a failed build.

### Gate and rehearsal

`gate` changes nothing live; it retains a report even on refusal:

| Step | Evidence required |
|---|---|
| 1 | Install manifest and hashes match; source-built debug/release provenance, not an unverified prebuilt; candidate is not rejected |
| 2 | Exact-commit detached worktree, sealed runtime and direct toolchain: `cargo test -p atmd --lib replay`, then `--lib frame_proof`, with `ATM_RECORD_REGISTRY_REPLAY` removed. Each must exit 0, fail zero and pass more than zero tests; counts are compared with stable's report when available. Goldens are not regenerated |
| 3 | `--gate-pass` is an existing full-gate runner record for that exact commit and the explicit `--gate-runner-label`: one passed outcome, no failed outcome, matching start/pass/exit heads, exit 0 and no leftovers. The full gate is **not rerun** |
| 4 | Private, disposable state and tmux, repository stand-in pi, no model CLI: stable performs up/message/clear/down, candidate repeats, stable starts again and answers list and agent list, then stops cleanly |
| 5 | `atm-rollout --dry-run`: protected-file report and its hash, plus the quiet snapshot |

`gate` requires `--gate-runner-label` with the record's exact, nonblank,
single-line runner label; omission is a usage error. It is matched literally,
including regex punctuation, with no default, autodetection, environment-variable
fallback or config-file reader. In the example, exit and outcome lines begin
`gate-runner exit ...` and `gate-runner: GATE PASSED ...`; the start line still
begins `gate start ...`. The selected label is retained in the hashed gate-pass
step command alongside the record evidence. Keep machine-local choices outside
the checkout and pass the label explicitly.

Rehearsal says **two-way** only if every phase and final stop succeeds; otherwise
**ONE-WAY**, with the failing step/output. ONE-WAY is not proof that the old build
can read newer state. It does not itself refuse promotion, but uncertain cleanup
does refuse gate. Rehearsal never copies live state.

The output names the retained report and its sha256. Read it, including every
step's command/result/hash, compatibility and the stable it replaces. Obtain the
owner's yes to **that supervisor report hash** before passing `--approve`. This
is not rollout's separate protected-report hash. `promote` verifies the unused
passed report, candidate and retained kit, repeats manifest checks and rollout's
dry-run, and refuses any protected-report/hash or quiet-snapshot difference. It
passes the protected-report hash to rollout itself. Approval does not automate
QUIET, stopping atm-calling helpers, reload reminders or re-confirming seats.

### Probation, bounded recovery and NEEDS YOU

After rollout exits 0, the supervisor pins its checked daemon's PID/start identity
and probes immediately, then every 15 seconds for 30 minutes (including the end
boundary). Peer identity is checked too: an autostarted replacement is not the
same daemon. The verdict uses the whole observation history:

- Failed reservations, stopped lifecycle, refused socket, dead process or changed
  identity: unhealthy on one read.
- Missing/malformed or `unavailable` answers while alive: unhealthy on three
  consecutive reads; a valid supported answer resets the streak. `unavailable`
  can mean stopped control, **not** an old build without the method.
- Only `unknown_method` is UNKNOWN before supported health has been seen.
  Acceptance explicitly relies on rollout checks. Losing previously supported
  health counts toward the invalid-answer streak.
- At the end, accept only a healthy or legacy UNKNOWN verdict, or full with an
  earlier ok. Full throughout ends NEEDS YOU. A pending final verdict ends NEEDS
  YOU `health remained uncertain at the end of probation`, even after earlier ok.

**One automatic rollback is authorised: UNHEALTHY probation, two-way candidates
only.** First copy both `atm.log` and `atm.log.1` into a private evidence directory.
Immediately reverify slots, the installed candidate and the saved last-known-good
pair (bytes and modes), then invoke `scripts/atm-rollout --rollback` once with the
explicit socket in `ATM_SOCKET`, under the same 900-second capture guard as the
forward rollout. Only rollback exit 0 with a verified stable pair and freshly
pinned daemon permits one stable health read. Healthy or legacy UNKNOWN means
**exit 3 rolled-back**; output includes R's state line, the restored pair, health
and diagnostic evidence. The candidate is rejected, approval stays used, and
stable/previous do not rotate. The supervisor itself starts nothing and makes
no retry or forward promotion; if a process starts using the daemon socket between
the quiet check and the swap, R refuses the swap and starts the installed candidate
again within its one call (as a manual `--rollback` does), then the supervisor exits
4 NEEDS YOU, and that restart can drop a failed candidate's quarantine tombstones.
The automatic path needs R's quiet moment during probation (no message in delivery,
agent start/clear or atm command running over five seconds); otherwise R exits 1,
the supervisor exits 4, and the candidate is rejected but still installed.

Everything else remains **NEEDS YOU, exit 4**: ONE-WAY, uncertain final health,
full throughout, failed/missing diagnostic copies, drift, refused/uncertain
rollback, or unhealthy/uncertain restored health. After an automatic attempt,
exit 4 prints R's exit and stderr, the rollback command only if the candidate pair
is still installed, and start preparation only if no daemon accepts on the socket
now; paths with no automatic attempt keep their existing manual advice.
The compatibility and manual command below are for intervention, not run again. ONE-WAY adds a warning:
the old build may not read the organisation log the newer build wrote; named
agents/messages may be unavailable until the newer build is back. Changed or
unverifiable slots stay untouched/pending rather than being overwritten with an
older snapshot. Boundary failures still retain both diagnostic logs and reject
without an automatic action. State is never copied back, cut or deleted.

```sh
scripts/atm-rollout --rollback --bin-dir /absolute/installed/bin --socket /absolute/daemon.sock
```

For dead/refused, the one authorized rollback attempt can refuse because rollout
itself requires an answering daemon: it says
`no daemon answers on <socket>; start it and run again`. The supervisor prints
these preparatory steps too, **for you to run**, never runs them itself:

```sh
ATM_SOCKET=/absolute/daemon.sock /absolute/installed/bin/atmd start -d
# Wait for the socket to answer, then run the printed scripts/atm-rollout --rollback.
```

Keep the same explicit socket/state/HOME/tmux environment. If the candidate cannot
start, do not retry or let a CLI autostart it: use the stopped-daemon
`atm-rollback` procedure above after verifying QUIET and the saved stable pair.

Three `missing` reads can also mean a living daemon lost its socket pathname.
That case prints only the rollback command, with no start preparation; rollout
still refuses `no daemon answers on <socket>; start it and run again`. Inspect
the pinned PID/start identity and executable, the selected socket path and daemon
logs. Do not start a second daemon or assume the first one stopped. Make helpers
quiet, stop only that verified daemon, then use the stopped-daemon rollback
procedure with a verified saved stable pair; do not unlink an unknown socket or
bypass its lock.

Inspect the evidence, installed pair and last-known-good hashes before running
that command yourself, with the same QUIET/environment requirements as rollout.
`stable` is the supervisor's last-known-good, not necessarily the pair selected
by promotion's `previous.json`: a mismatch means **do not roll back**. Manual
rollout does not reconcile supervisor slots; after rolling back an accepted
candidate, status may report installed-pair drift. Investigate rather than
reinitializing or editing slots to hide it.

| NEEDS YOU condition | Exit / action |
|---|---|
| Rollout exits 4, or its completion is uncertain | 4; nothing further tried |
| Unhealthy probation, **ONE-WAY** | 4; diagnostic logs, rejection and manual command only |
| Two-way recovery lacks either diagnostic copy, rollback refuses/fails, or completion is uncertain | 4; retained evidence and manual steps, no retry/start |
| Last-known-good pair/hash mismatch | 4; no rollback |
| Installed pair is neither stable nor pending candidate, or rollout installed an unexpected pair | 4; no swap/retry |
| Old build cannot read the newer organisation log | Rollout 4, supervisor 4; state left in place |
| Stable health is unhealthy or uncertain after automatic rollback or rollout's own exit-3 recovery | 4; no further recovery |
| Full throughout probation, or final health remains uncertain | 4; diagnostic logs, rejection and manual command |
| Unreadable/invalid slots or retained artifacts; kit cannot be verified | 4; no repair |
| Probation interrupted, daemon cannot be pinned, or post-swap decision cannot be persisted | 4; investigate installed state, no automatic action |
| Stable unhealthy outside promotion (`check`) | **2**; report only, no restart |

Rollout's existing internal recovery is unchanged: if it already exits 3, the
supervisor reads stable health once and exits 3 only if healthy/UNKNOWN; it never
requests a second recovery. The new automatic path is only after a successful
forward rollout and an unhealthy two-way probation. Successful probation
accepts/rotates and exits 0.
Preflight/approval refusal exits 1; usage exits 2. `check` makes one read only:
0 healthy, 2 unhealthy, 4 uncertain (including UNKNOWN/full or pending candidate
installed). It changes no slots and never acts on an unhealthy stable daemon.

### Known limits and the manual way out

- **No resume or slot-reconciliation command.** Abrupt supervisor exit can leave
  candidate pending indefinitely. Ctrl-C or a failed decision can leave the
  candidate installed but rejected while stable still names the old pair.
  `promote` refuses a pending candidate; `check` is not an acceptance command.
  Preserve the metadata/evidence and verify the installed pair and last-known-good
  before manual rollback. A manual rollback does not clear pending slots or undo
  an accepted slot rotation.
- **Post-swap parsing/identity errors can leave candidate pending.** A ValueError
  or KitRefused (for example corrupt `previous.json`, or malformed proc identity
  during probation or the exit-3 health read) exits 4 NEEDS YOU. The original
  forward-rollout parsing/identity path prints no rollback command or captured
  result. Automatic recovery's verification/read failures retain its captured
  evidence and manual steps, but can also leave candidate pending. With corrupt
  `previous.json`, rollout's `--rollback` refuses too; repeating it cannot repair
  the marker.
  Inspect the actual installed pair and retained stable install. With helpers
  quiet and the verified daemon stopped, use the stopped-daemon `atm-rollback`
  procedure only if its saved pair/marker can still be verified. Otherwise use
  the verified immutable stable install and fresh-bin baseline below; do not
  guess at the saved pair or edit checksummed slots to clear the pending record.
- **The 900-second guard on either rollout call is not a recovery transaction.**
  Guard expiry or Ctrl-C kills R's subprocess and retains captured output in
  `rollout` (forward) or `rollback` (automatic recovery), but R may
  not have emitted its state line and work it launched may still be in flight.
  Inspect actual processes, daemon/socket, pair and logs; wait for that work to
  finish before a manual action. Do not infer that a timeout means nothing changed.
  If no daemon answers, use the printed preparation above; if it cannot start,
  follow the stopped-daemon rollback procedure, not repeated promotion attempts.
- **A post-staging rollout refusal burns approval and rejects the build**, even
  when R exits 1 before swapping. The same rejected commit/pair cannot be gated
  again while retained in rejected slots; a fresh hash alone does not bypass it.
  Use a corrected candidate, not approval/rejection edits or artificial failures
  to evict the record.

If recovery leaves old metadata inconsistent or pending, retain it intact for
investigation. There is no supported in-place command to repair that lineage.
After verified recovery, a manual fresh baseline needs a **new bin directory**,
the verified stable install's bytes **and modes**, and the same immutable kit
install. For example, with the daemon stopped and helpers quiet:

```sh
mkdir -m 700 /absolute/recovery/bin
cp -p /absolute/retained/stable/bin/atm /absolute/recovery/bin/atm
cp -p /absolute/retained/stable/bin/atmd /absolute/recovery/bin/atmd
scripts/atm-supervise init --installed-commit STABLE_FULL_SHA \
  --install STABLE_FULL_SHA-debug --bin-dir /absolute/recovery/bin
```

Keep the original bin/metadata, reports, rejected records and used approvals;
do not copy a pending `slots.json`, delete the old metadata or hand-edit its
checksum. Start the recovered daemon from the new directory only after verifying
no old daemon remains, with the same socket and persistent state, then check its
lists/health before gating a corrected candidate. This explicit operator reset
is not a retry authorisation or an automatic recovery feature.

## Verification

`cargo test --test candidate_kit` and `cargo test --workspace` run the candidate
kit suite, `promotion_kit_test.py`, `rollout_kit_test.py` and
`supervisor_kit_test.py` through the Cargo bridge. For
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
