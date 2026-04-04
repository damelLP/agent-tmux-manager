# Agent Tmux Manager — Roadmap

## Status: v0.1.5 — MVP Complete, In Daily Use

An htop-style TUI for monitoring Claude Code agents across tmux sessions.
Daemon-based architecture with Unix socket IPC, built in Rust with ratatui.

## What's Shipped

| Phase | Summary | Tests |
|-------|---------|-------|
| 1. Core Infrastructure | Daemon, registry, Unix socket IPC, session lifecycle | ~150 |
| 2. Shell Integration | Hook scripts, status line parsing, /proc discovery | ~100 |
| 3. TUI | Session list, detail view, keybindings, real-time updates | ~100 |
| 4. Polish | 3-state status model, PID-only cleanup, error handling | ~75 |

**425 tests passing** | **Version: v0.1.5**

## What's Next

### Near-term

- [ ] Projects — group sessions by CWD ([#2](https://github.com/damelLP/agent-tmux-manager/issues/2))
- [ ] Teams/Subagents — parent-child relationships ([#3](https://github.com/damelLP/agent-tmux-manager/issues/3))
- [ ] Filtering — by status, agent type, search ([#4](https://github.com/damelLP/agent-tmux-manager/issues/4))
- [ ] Preview pane — richer detail view ([#5](https://github.com/damelLP/agent-tmux-manager/issues/5))
- [ ] Vim navigation — gg, G, Ctrl-d/u, etc. ([#6](https://github.com/damelLP/agent-tmux-manager/issues/6))
- [ ] Session actions — kill with confirmation ([#7](https://github.com/damelLP/agent-tmux-manager/issues/7))
- [ ] Pre-select current pane — auto-highlight last session ([#8](https://github.com/damelLP/agent-tmux-manager/issues/8))

### Later

- [ ] Help screen — ? key ([#9](https://github.com/damelLP/agent-tmux-manager/issues/9))
- [ ] Config file — ~/.config/atm/config.toml ([#10](https://github.com/damelLP/agent-tmux-manager/issues/10))
- [ ] User documentation & README polish ([#11](https://github.com/damelLP/agent-tmux-manager/issues/11))
- [ ] Binary packaging / release automation ([#12](https://github.com/damelLP/agent-tmux-manager/issues/12))

## Architecture

The system has three components: **atmd** (daemon that discovers and tracks sessions),
**atm** (TUI client that connects via Unix socket), and **hook scripts** (bash scripts
installed as Claude Code hooks that push real-time events to the daemon).
See `docs/` for detailed architecture documentation.
