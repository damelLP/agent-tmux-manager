# Agent Tmux Manager (ATM)

[![Build Status](https://github.com/damelLP/agent-tmux-manager/actions/workflows/release.yml/badge.svg)](https://github.com/damelLP/agent-tmux-manager/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Real-time management for Claude Code agents across tmux sessions.

![ATM Screenshot](assets/screenshot.png)

## What it does

ATM gives you a live dashboard and CLI for every Claude Code agent running in tmux. See context usage, cost, model, and activity at a glance — and control agents without switching panes.

- **Dashboard** — real-time TUI with session tree, context bars, cost tracking, and live terminal capture
- **Agent control** — spawn, kill, interrupt, send text, and reply to prompts from the CLI
- **Workspaces** — create tmux sessions with built-in ATM sidebars, or inject sidebars into existing sessions
- **Layouts** — preset multi-agent arrangements (solo, pair, squad, grid) with one command
- **Tmux native** — status bar integration, popup picker, vim-style keybindings

## Install

```bash
curl -sSL https://raw.githubusercontent.com/damelLP/agent-tmux-manager/main/scripts/install.sh | sh
```

Or via Cargo:

```bash
cargo install atm && atm setup
```

## Quick start

```bash
atm                    # launch TUI (starts daemon automatically)
```

Sessions appear as you use Claude Code. Press `Enter` to jump to any session, `q` to quit.

## CLI at a glance

```bash
atm spawn -m opus -d right         # spawn agent with model and direction
atm kill <id>                      # kill agent and close pane
atm interrupt <id>                 # Ctrl+C an agent
atm send <id> "fix the tests"     # send text to agent
atm reply <id> --yes               # accept a permission prompt
atm peek <id> --prompt             # extract the active prompt
atm list -f json --status working  # list working agents as JSON
atm status                         # one-line summary for tmux status bar

atm workspace create               # new session with ATM sidebar + agent + shell
atm workspace attach               # inject sidebar into current session
atm layout pair                    # two agents + ATM sidebar
```

## How it works

```
Claude Code  ──hook──▶  atmd (daemon)  ◀──socket──  atm (TUI/CLI)
```

`atm setup` registers hooks in `~/.claude/settings.json`. Claude Code fires events on every tool use, status update, and lifecycle change. The `atm-hook` script forwards these to the `atmd` daemon over a Unix socket, and `atm` connects for real-time display.

## Documentation

See the **[Wiki](https://github.com/damelLP/agent-tmux-manager/wiki)** for the full user guide, tmux integration, architecture, and troubleshooting.

## Building from source

```bash
git clone https://github.com/damelLP/agent-tmux-manager.git
cd agent-tmux-manager
cargo build --release
```

## License

MIT — see [LICENSE](LICENSE).
