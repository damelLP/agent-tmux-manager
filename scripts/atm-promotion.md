# Promotion tools (Linux, Python 3.9+)

## Protected-file report

From the repository root:

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

The rename pair is deliberately **not atomic**. SIGKILL or power loss between
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
command still exits nonzero and reports recovery success or failure. Failure
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

## Verification

`cargo test --test candidate_kit` and `cargo test --workspace` run both the
candidate kit suite and `promotion_kit_test.py` through the Cargo bridge. For
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
setup against Cargo-built pairs, never a model harness. Same-version pairs prove
promotion/rollback mechanics, not a real historical downgrade or state recovery.
