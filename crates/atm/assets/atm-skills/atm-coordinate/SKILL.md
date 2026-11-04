---
name: atm-coordinate
description: Coordinator loop for long-lived implementer agents in other tmux panes, driven only through the atm CLI. Plan tasks with the user, send each one as a brief file, wait, read the report, review, and clear the implementer before its next task. Only when the user invokes it, as /atm-coordinate (Claude Code), /skill:atm-coordinate (pi) or $atm-coordinate (Codex).
argument-hint: "[--profile <name>]"
disable-model-invocation: true
---

# atm coordinator

You coordinate implementer agents that run in other tmux panes. You and the
user split the work into tasks. You send each task to an implementer, wait for
it, read its report and review the work. Do all of it through `atm`. When atm
refuses something or does what you didn't expect, read that command's section
in the [command reference](references/atm-commands.md): it says why and
what to do instead.

The loop, for each task: plan it with the user (1); check the implementer's
row, clear it, wait for its new session and send the skill call (2); wait for
the turn to end (3); read the last line of its report and answer it (4); and
check your own context (5).

Your arguments: Claude Code puts them here: $ARGUMENTS
(pi and Codex leave that placeholder as it is; there, they're the rest of the
message that invoked this skill.)

Run this skill's scripts as `bash <this skill's folder>/scripts/<script>`,
from your own working directory: `atm-profile` searches from the current
directory.

**Quote what you send with single quotes:** `atm send %N '...'`. In double
quotes your shell expands `$atm-implement` to `-implement` and runs anything
in backticks. Write an apostrophe as `'\''`.

## 0. Before the first task

1. **Check atm.** `atm send --help` must mention "bracketed paste" (#119). If it
   doesn't, stop and tell the user: this atm types text instead of pasting it,
   which turns every newline into Enter. They need a newer build.
2. Check that `jq` is on `PATH`, `$TMUX_PANE` is set (you run in tmux), and
   `atm list -f json` works from your shell tool. Codex's default sandbox
   blocks atm's unix sockets ("Operation not permitted"): ask the user to
   allow them (see the command reference) rather than working around it.
3. **Profile.** With `--profile <name>`, run
   `bash <this skill's folder>/scripts/atm-profile <name>` and follow the
   profile it prints. These rules win over it.
4. **Implementers.** Ask the user which panes are your implementers, unless the
   user or the profile already said. Spawn one only when the user asks or
   agrees; you may suggest it, for example when every implementer is busy:
   `atm spawn -H <harness> -w <dir> -t "$TMUX_PANE"`, then find its pane (see
   the reference). Keep a table: name, pane (`%N`), harness, `project_root`,
   worktree, task, brief file. (`working_directory` in `atm list` is cut to
   30 characters, so record `project_root` and `worktree_path`.)

## Per harness

Look up the implementer's `harness` in `atm list -f json`.

| Implementer | Clear it with | Invoke the skill | The skill call |
|---|---|---|---|
| `claude` | `/clear` | `/atm-implement` | up to 800 characters and 3 lines, or the skill doesn't load |
| `pi` | `/new` (aborts a busy turn) | `/skill:atm-implement` | any length |
| `codex` | `/new` | `$atm-implement` | any length |
| anything else, or no atm skills installed there (`atm setup` installs them for Claude Code, pi and Codex) | ask the user | none: start the brief with "Read `<path>` and follow it", where `<path>` is the absolute path of `../atm-implement/SKILL.md` from this skill's folder; name the profile's file in it, and send `Your task is in <brief path>. Read it and follow it.` | any length |

Send and wait in one shell command, so a short turn can't end between the
two: `atm send %N '...' && bash <this skill's folder>/scripts/atm-wait %N`.
How you wait depends on your own harness:

- **Claude Code:** run that command in the background; you're told when it
  ends.
- **pi, Codex and others:** run it in the foreground with a timeout your shell
  tool allows, such as `atm-wait %12 300`. After `TIMEOUT`, run
  `atm-wait %12 300 0` to keep waiting for the same turn, with `--after-reply`
  only if a peek shows the tool you approved still running, not a new dialog.

## 1. Plan with the user

Split the work into tasks together. Each task gets a standalone brief, because
the implementer starts it from a cleared context: the goal, the agreed design,
constraints, when it's done, and how to report. Add what the profile says each
brief needs.

Write each brief to any file the implementer can read and git doesn't track,
for example `/tmp/atm-briefs/<project>/<implementer>-<n>.md`, or a folder
the project's `.gitignore` excludes (in atm's own repo, `docs/plans/`). A folder per project, and numbers that
count on across handovers, keep a brief from being overwritten while an
implementer may still read it. One task per implementer at a time.

## 2. Send a task

1. **Check the row right before the clear,** even if `atm-wait` just returned
   `IDLE`: a Stop hook with `asyncRewake` can wake a session after its turn
   ends.
   - It must read `idle`. If it's busy, wait (step 3) or pick another
     implementer. After `atm interrupt`, trust a peek that shows no turn
     running: Claude's row keeps reading `working` (see the reference).
   - Its `harness` and `project_root` must match your table: after a tmux
     server restart, or in a second tmux server, the same `%N` can name
     someone else's session. If they don't match, stop and ask the user.
     (`worktree_path` may change as the implementer moves to a new worktree.)
   - Note its `id`.
2. **Peek.** `atm peek %N -n 15`: no dialog, and no turn running.
3. **Clear.** `atm send %N <clear command>`.
4. **Wait for the new session.** A clear starts a new session, which atm shows
   as a new `id` on the same pane. Poll `atm list -f json` about once a second
   until the pane's `id` differs from the one you noted, then send at once.
   Don't send sooner: a send milliseconds after the clear can reach the
   harness merged with it, as one plain-text message, and nothing is cleared.
   Don't wait for an empty input line either: Claude shows the command you
   just ran in grey (`❯ /clear`), and a peek can't tell that from typed text.
   If no new `id` appears within about 10 s, peek: if the previous task's
   output is gone and no turn is running, the clear ran; otherwise ask the
   user.
5. **Send the skill call** on one line, in single quotes, and wait (step 3)
   in the same command:
   `atm send %N '<invoke> [--profile <name>] <brief file>' && bash <this skill's folder>/scripts/atm-wait %N`,
   for example
   `atm send %7 '/atm-implement --profile pr-implementer /tmp/atm-briefs/myproject/alpha-3.md' && bash <this skill's folder>/scripts/atm-wait %7`.

## 3. Wait

`bash <this skill's folder>/scripts/atm-wait %N` polls `atm list -f json`
until the turn ends. It prints one word:

- `IDLE`: the turn ran and ended. Go to step 4.
- `ATTENTION`: a dialog waits. Go to step 4.
- `GONE`: the implementer's session is gone. Peek, then tell the user.
- `NEVER_STARTED`: the row never went working. Peek: the send may have failed,
  or the turn was shorter than a poll.
- `TIMEOUT`: still working. Peek to see whether it's stuck, then wait again
  with a start timeout of 0, and `--after-reply` only if the peek shows the
  tool you approved still running, not a new dialog.

## 4. Read and answer

`atm peek %N -n 60` (it shows only the visible screen, so a small pane shows
fewer lines). The last line of the implementer's final message, just above its
input box, starts with one of:

- **`QUESTION:`** Answer it when the answer is inside what you and the user
  agreed; otherwise ask the user. Send the answer and wait again in one
  command, `atm send %N '<answer>' && bash <this skill's folder>/scripts/atm-wait %N`.
  An answer may be any length: only a skill call has to be
  short. For a long one, write a file and send its path in a sentence.
- **`BLOCKED:`** Unblock it if that's clearly yours to do; otherwise tell the
  user.
- **`WAITING:`** The implementer ended its turn while a job of its own (CI, a
  long test) still runs in the background; it reports in the next turn. Wait
  again with a long start timeout, for example `atm-wait %N 3600 1800`: the
  default 120 s would end `NEVER_STARTED` before a longer job finishes.
- **`DONE: plan <path>`** The implementer stopped for your OK. Read the plan,
  then send your OK or your changes and wait again, in one command as for an
  answer. Don't
  clear it: the task isn't done.
- **`DONE:`** with a result: review it as the profile says. Only now is the
  implementer free for its next task (step 2).

If there's a dialog instead, run `atm peek %N --prompt`. It extracts a dialog
on a best-effort basis, so confirm with a plain `atm peek %N -n 40`. Answer it
with `atm reply` only when you know what you're approving and it's inside the
agreed task; otherwise ask the user.

- After you approve (`atm reply -y`, or `-o N` for an option you know
  approves), wait with `atm-wait --after-reply %N`: Claude's row reads
  `attention_needed` until the approved tool ends.
- After `atm reply -n`, `atm send` refuses Claude's row until its next prompt:
  ask the user to type the next instruction in that pane (until #127).

If there's neither a dialog nor one of those lines: when the screen shows a
background job still running (Claude Code says "1 shell still running"),
treat it as `WAITING:`. Otherwise the turn ended without a report, and the
implementer won't start another turn by itself. Send it one reminder and
wait again, in one command:
`atm send %N 'End your turn with one line starting QUESTION:, BLOCKED:, WAITING: or DONE:.' && bash <this skill's folder>/scripts/atm-wait %N`.
If that turn ends without one either, ask the user.

## 5. Hand over at 40% context

After each report (step 4), read your own context from atm:
`atm list -f json | jq '.[] | select(.tmux_pane == env.TMUX_PANE and .parent_session_id == null) | .context_percentage'`.
That's your real context as your harness reports it, so trust it over your
own sense of how much you've read. Past 40, write a handover before the next
task: the state of each task, each implementer (name, pane, harness,
`project_root`, worktree, task, brief file), decisions made, and open
questions. Ask the user once where to write it, unless the profile or an
earlier handover says. Then tell the user.

If you find no row of your own, or it reads 0 after real work, atm doesn't
know your context (see the reference): use what your harness shows you, and
if it shows nothing, ask the user at the next task boundary.

## Never

- Never press a feedback survey's number keys: that answers the harness's
  survey on the user's behalf.
- Never take text suggested in an input box as the user's OK.
- Never touch sessions that aren't your implementers.
- Never drive an implementer with raw tmux: `atm`'s commands carry the
  refusals.
- Kill implementers (`atm kill %N`) only when the user asks or the loop is over.

The user may type into an implementer's pane, for example to answer it
directly. A draft left there merges with your next send, and atm can't see it.
