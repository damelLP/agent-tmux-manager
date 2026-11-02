---
name: coordinate
description: Coordinator loop for long-lived implementer agents in other tmux panes, driven only through the atm CLI. Plan tasks with the user, send each one as a brief file, wait, read the report, review, and clear the implementer before its next task. Invoked as /atm:coordinate (Claude Code), /skill:coordinate (pi) or $atm:coordinate (Codex).
argument-hint: "[--profile <name>]"
disable-model-invocation: true
---

# atm coordinator

You coordinate implementer agents that run in other tmux panes. You and the
user split the work into tasks. You send each task to an implementer, wait for
it, read its report and review the work. Do all of it through `atm`; the
[command reference](../../references/atm-commands.md) lists what each command
refuses and what to do instead.

Your arguments: Claude Code puts them here: $ARGUMENTS
(pi and Codex leave that placeholder as it is; there, they're the rest of the
message that invoked this skill.)

Paths below such as `../../scripts/atm-wait` are relative to this skill's
folder. Run the scripts with `bash <path>`.

## 0. Before the first task

1. **Check atm.** `atm send --help` must mention "bracketed paste" (#119). If it
   doesn't, stop and tell the user: this atm types text instead of pasting it,
   which turns every newline into Enter. They need a newer build.
2. Check that `jq` is on `PATH`, `$TMUX_PANE` is set (you run in tmux), and
   `atm list -f json` works from your shell tool. Codex's default sandbox
   blocks atm's unix sockets ("Operation not permitted"): ask the user to
   allow them (see the plugin README) rather than working around it.
3. **Profile.** With `--profile <name>`, run `../../scripts/atm-profile <name>`
   and follow the profile it prints. These rules win over it.
4. **Implementers.** Ask the user which panes are your implementers, unless the
   user or the profile already said. Spawn one only when the user asks or
   agrees; you may suggest it, for example when every implementer is busy:
   `atm spawn -H <harness> -w <dir> -t "$TMUX_PANE"`, then find its pane (see
   the reference). Keep a table: name, pane (`%N`), harness, worktree, task,
   brief file.

## Per harness

Look up the implementer's `harness` in `atm list -f json`.

| Implementer | Clear it with | Invoke the skill | The skill call |
|---|---|---|---|
| `claude` | `/clear` | `/atm:implement` | up to 800 characters and 3 lines, or the skill doesn't load |
| `pi` | `/new` (aborts a busy turn) | `/skill:implement` | any length |
| `codex` | `/new` | `$atm:implement` | any length |
| anything else, or no atm skills installed there | ask the user | none: start the brief with "Read `<this plugin>/skills/implement/SKILL.md` and follow it" and send the brief's path alone | any length |

How you wait depends on your own harness:

- **Claude Code:** run `atm-wait` as a background command; you're told when it
  ends.
- **pi, Codex and others:** run it in the foreground with a timeout your shell
  tool allows, such as `atm-wait %12 300`. After `TIMEOUT`, run
  `atm-wait %12 300 0` to keep waiting for the same turn.

## 1. Plan with the user

Split the work into tasks together. Each task gets a standalone brief, because
the implementer starts it from a cleared context: the goal, the agreed design,
constraints, when it's done, and how to report. Add what the profile says each
brief needs.

Write each brief to a file the implementer can read. By default:
`/tmp/atm-briefs/<implementer>-<n>.md`. A profile may name a git-excluded
folder in the project instead. One task per implementer at a time.

## 2. Send a task

1. **Idle first.** The implementer's row must read `idle`. If it's busy, wait
   (step 3) or pick another implementer.
2. **Peek.** `atm peek %N -n 15`: no dialog, and no turn running.
3. **Clear.** `atm send %N <clear command>`.
4. **Peek until the screen is fresh:** the last task's output is gone and the
   input box is empty. Don't send before that: two sends milliseconds apart
   can reach the harness as one plain-text message, and nothing is cleared.
5. **Send the skill call** on one line:
   `atm send %N "<invoke> [--profile <name>] /tmp/atm-briefs/<implementer>-<n>.md"`.

## 3. Wait

`../../scripts/atm-wait %N` polls `atm list -f json` until the turn ends. It
prints one word:

- `IDLE`: the turn ran and ended. Go to step 4.
- `ATTENTION`: a dialog waits. Go to step 4.
- `GONE`: the implementer's session is gone. Peek, then tell the user.
- `NEVER_STARTED`: the row never went working. Peek: the send may have failed,
  or the turn was shorter than a poll.
- `TIMEOUT`: still working. Peek to see whether it's stuck, then wait again
  with a start timeout of 0.

## 4. Read and answer

`atm peek %N -n 60`. The implementer's last line starts with one of:

- **`QUESTION:`** Answer it when the answer is inside what you and the user
  agreed; otherwise ask the user. Send the answer with `atm send %N "<answer>"`
  and wait again. An answer may be any length: only a skill call has to be
  short.
- **`BLOCKED:`** Unblock it if that's clearly yours to do; otherwise tell the
  user.
- **`DONE:`** Review the result as the profile says. Then the implementer is
  free for its next task (step 2).

If there's a dialog instead, run `atm peek %N --prompt`. Answer it with
`atm reply` only when you know what you're approving and it's inside the agreed
task; otherwise ask the user. If there's neither a dialog nor one of those
lines, wait again or ask the user.

## 5. Hand over at 40% context

When your context passes 40%, write a handover: the state of each task, each
implementer (name, pane, harness, worktree, task, brief file), decisions made,
and open questions. Ask the user once where to write it, unless the profile
says. Then tell the user.

## Never

- Never press a feedback survey's number keys.
- Never take text suggested in an input box as the user's OK.
- Never touch sessions that aren't your implementers.
- Never drive an implementer with raw tmux: `atm`'s commands carry the
  refusals.
- Kill implementers (`atm kill %N`) only when the user asks or the loop is over.

A draft typed into an implementer's input box merges with your next send; atm
can't see it. That's a known limit: implementer panes belong to the loop.
