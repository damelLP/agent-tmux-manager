# atm skills

Two skills that let one agent coordinate others through the `atm` CLI, for
any harness atm supports:

- **`coordinate`**: the coordinator loop. Plan tasks with the user, send each
  one to a long-lived implementer, wait for it, read its report, review, and
  clear the implementer before its next task.
- **`implement`**: the implementer's rules. One task, in its own worktree,
  and every turn ends with a `QUESTION:`, `BLOCKED:`, `WAITING:` or `DONE:`
  line the coordinator can read.

Everything goes through atm (`atm list`, `peek`, `send`, `reply`, `spawn`),
so the loop is the same whichever harness runs on either side. The commands,
their refusals and what to do instead are in
[`references/atm-commands.md`](references/atm-commands.md).

This is v0 (#123): it works with today's CLI. `atm role` (#148), names
(#118), `atm profile show` (#147) and spawn output (#121) will replace the
workarounds noted in the skills.

## Prerequisites

- **`atm` built after #119.** Its `atm send` pastes text and refuses unsafe
  sends. Older builds type text with `send-keys`, which turns each newline into
  Enter. Check: `atm send --help` mentions "bracketed paste". The coordinate
  skill checks this first and stops if it's missing.
- `atm setup` run for each harness you use, so the sessions show in `atm list`.
- `tmux`, and `jq` on `PATH` for the coordinator.

## Install

The plugin lives in this folder. The repository root holds a marketplace
(`.claude-plugin/marketplace.json`) that points at it. Replace
`<repo>` with your checkout of agent-tmux-manager.

| Harness | Install | Invoke |
|---|---|---|
| Claude Code | `claude plugin marketplace add <repo>`<br>`claude plugin install atm@agent-tmux-manager` | `/atm:coordinate`, `/atm:implement` |
| pi | add `"skills": ["<repo>/extensions/atm-skills/skills"]` to `~/.pi/agent/settings.json`, or start pi with `--skill <repo>/extensions/atm-skills/skills` | `/skill:coordinate`, `/skill:implement` |
| Codex CLI | `codex plugin marketplace add <repo>`<br>`codex plugin add atm@agent-tmux-manager` | `$atm:coordinate`, `$atm:implement` |

- **Claude Code** installs at user scope by default, so every session sees the
  skills, including implementers that `atm spawn` starts in other directories.
  A local marketplace is read from the folder itself, so edits to the skills
  apply to new sessions without reinstalling.
- **pi**: `atm spawn -H pi` doesn't pass `--skill`, so use the settings entry
  for spawned implementers.
- **Codex** copies the plugin into `$CODEX_HOME/plugins/cache/`. Run
  `codex plugin add atm@agent-tmux-manager` again after pulling changes.

### A Codex coordinator needs its sandbox opened

A coordinator runs `atm` from its shell tool. In Codex 0.163.0's default
`workspace-write` sandbox, `atm list` fails with "Failed to connect to daemon
... Operation not permitted": the sandbox blocks the connection to atmd's unix
socket. With network access allowed, `atm list`, `peek` and `send` all worked:

```toml
# ~/.codex/config.toml, or -c sandbox_workspace_write.network_access=true
[sandbox_workspace_write]
network_access = true
```

That also lets the coordinator's other commands reach the network. Approving
each `atm` call to run outside the sandbox is the narrower option. An
implementer never runs `atm`, so it needs neither.

## Profiles

`--profile <name>` adds a reusable brief to either skill: a Markdown file
`<name>.md` in `.atm/profiles/` (the first one found walking up from the
current directory) or in atm's config dir (`~/.config/atm/profiles/` on
Linux). The project's file wins; they aren't merged. This repository's
profiles are in [`/.atm/profiles/`](../../.atm/profiles/).
