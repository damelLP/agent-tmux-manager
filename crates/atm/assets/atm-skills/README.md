# atm skills

Two skills that let one agent coordinate others through the `atm` CLI, for
any harness atm supports:

- **`atm-coordinate`**: the coordinator loop. Plan tasks with the user, send
  each one to a long-lived implementer, wait for it, read its report, review,
  and clear the implementer before its next task.
- **`atm-implement`**: the implementer's rules. One task, in its own worktree,
  and every turn ends with a `QUESTION:`, `BLOCKED:`, `WAITING:` or `DONE:`
  line the coordinator can read.

Everything goes through atm (`atm list`, `peek`, `send`, `reply`, `spawn`),
so the loop is the same whichever harness runs on either side. The commands,
their refusals and what to do instead are in
[`shared/references/atm-commands.md`](shared/references/atm-commands.md).

This is v0 (#123): it works with today's CLI. `atm role` (#148), names
(#118), `atm profile show` (#147) and spawn output (#121) will replace the
workarounds noted in the skills.

## Prerequisites

- **`atm` built after #119.** Its `atm send` pastes text and refuses unsafe
  sends. Older builds type text with `send-keys`, which turns each newline into
  Enter. Check: `atm send --help` mentions "bracketed paste". The
  `atm-coordinate` skill checks this first and stops if it's missing.
- `atm setup` run for each harness you use: it installs atm's hooks, so the
  sessions show in `atm list`, and these skills.
- `tmux`, and `jq` on `PATH` for the coordinator.
- A Codex coordinator needs its sandbox opened for atm's socket: see the
  [command reference](shared/references/atm-commands.md#a-codex-coordinator-needs-its-sandbox-opened).

## Install

`atm setup` installs both skills for each harness it finds, as plain skill
folders: no plugin, no marketplace and no settings entry. The files are built
into the `atm` binary, so `cargo install` and then `atm setup` is all it takes.
Run `atm setup` again after updating atm to update the skills. `atm uninstall`
removes them.

| Harness | Installed in | Invoke |
|---|---|---|
| Claude Code | `~/.claude/skills/atm-coordinate/`, `~/.claude/skills/atm-implement/` | `/atm-coordinate`, `/atm-implement` |
| pi | `~/.pi/agent/skills/atm-coordinate/`, `.../atm-implement/` | `/skill:atm-coordinate`, `/skill:atm-implement` |
| Codex CLI | `~/.codex/skills/atm-coordinate/`, `.../atm-implement/` | `$atm-coordinate`, `$atm-implement` |

- Each installed skill folder holds its `SKILL.md` and a copy of everything in
  `shared/`: the command reference, the scripts, and `agents/openai.yaml`.
  That file keeps Codex from invoking the skills on its own, since Codex
  ignores `disable-model-invocation`. Claude Code and pi ignore it.
- Claude Code and Codex list a newly installed skill in sessions that are
  already running. Restart pi sessions.
- The folders are the harnesses' defaults. With `CLAUDE_CONFIG_DIR` or
  `CODEX_HOME` pointing elsewhere, that harness doesn't read them, as with
  atm's hooks.
- `atm setup` replaces atm's two folders whole, so edits there, or a symlink
  in their place, are lost on its next run. To try a change to the skills,
  edit them here, rebuild atm and run `atm setup` again.

## Layout

- `atm-coordinate/SKILL.md`, `atm-implement/SKILL.md`: the skills.
- `shared/`: copied into every installed skill folder at the same paths.
- `tests/scripts.sh`: tests for the scripts, which `cargo test` runs. It and
  this README aren't installed.

## Profiles

`--profile <name>` adds a reusable brief to either skill: a Markdown file
`<name>.md` in `.atm/profiles/` (the first one found walking up from the
current directory) or in atm's config dir (`~/.config/atm/profiles/` on
Linux). The project's file wins; they aren't merged. This repository's
profiles are in [`/.atm/profiles/`](../../../../.atm/profiles/).

## Tests

`tests/scripts.sh` tests the scripts against a stand-in `atm` (`cargo test`
runs it). [`evals/atm-skills/`](../../../../evals/atm-skills/) holds decision tests for the
skills themselves: sealed, billed `claude -p` runs, each checking what a
coordinator or implementer does at one moment of the loop.
