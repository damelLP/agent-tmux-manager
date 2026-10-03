# Agent Tmux Manager (ATM)

[![Build Status](https://github.com/damelLP/agent-tmux-manager/actions/workflows/release.yml/badge.svg)](https://github.com/damelLP/agent-tmux-manager/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Real-time management for coding agents across tmux sessions.

## What it does

ATM gives you a live dashboard and CLI for coding agents running in tmux, including Claude Code, pi, and Codex CLI. See context usage, cost, model, and activity at a glance — and control agents without switching panes.

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

Sessions appear as you use supported coding-agent harnesses. Press `Enter` to jump to any session, `q` to quit.

## CLI at a glance

```bash
atm spawn -m opus -d right         # spawn default harness with model and direction
atm spawn --harness pi             # spawn a specific harness (pi, codex, ...)
ATM_SPAWN_PI_BIN=mise ATM_SPAWN_PI_ARGS='x pi' atm spawn --harness pi
```

ATM auto-creates `~/.config/atm/config.toml` with defaults when spawn config is first loaded. Default spawn harness and per-harness spawn defaults can be configured there:

```toml
[harness]
default = "pi"

[harness.pi]
binary = "mise"
default_args = ["x", "pi"]
```

Environment overrides still take precedence when set (`ATM_SPAWN_PI_BIN/ARGS`, then legacy `ATM_SPAWN_BIN/ARGS`); otherwise config values are used, then built-in defaults.

```
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
Claude Code / pi / Codex  ──hook/extension──▶  atmd (daemon)  ◀──socket──  atm (TUI/CLI)
```

`atm setup` registers supported harness integrations (Claude Code hooks, the pi extension, and Codex CLI hooks). Harness events are forwarded to the `atmd` daemon over a Unix socket, and `atm` connects for real-time display. Codex context usage is read on a best-effort basis from the rollout transcript supplied with hook events; a missing or changed transcript never blocks lifecycle updates. Codex requires a one-time trust approval of the installed hooks: run `/hooks` inside codex after `atm setup`.

## Daemon socket

`atmd` listens on a Unix socket at `/tmp/atm.sock` by default. Set `ATM_SOCKET` to move it. The daemon, the `atm` TUI and CLI, the Claude Code and Codex hooks, and the pi extension all read the same variable, and `atm` forwards it to the daemon it auto-starts, so one exported value keeps every component on the same socket:

```bash
export ATM_SOCKET=/run/user/1000/atm/atm.sock
atm                    # starts atmd on that socket if it is not running
atmd status            # prints the socket path once the daemon is up
```

The daemon creates the socket's parent directory on start. An empty `ATM_SOCKET` is treated as unset. Hooks read the variable from the agent's environment, which inherits from the tmux server, so export it before starting tmux or set it globally with `tmux set-environment -g ATM_SOCKET <path>`.

### Running agents in a devcontainer

Run `atm` and `atmd` on the host as usual and let harnesses inside the container report to the host daemon through a bind mount. Three things need to line up:

1. **Share the socket by directory, not by file.** A bind-mounted socket file pins one inode: when the host daemon restarts it recreates the socket and the container keeps the dead one. Point `ATM_SOCKET` at a file inside a mounted directory on both sides, and start the host daemon before the container so the directory exists. The container user needs write access to the socket; matching the host UID is the simplest way.
2. **Share the host PID namespace.** The daemon keys sessions by PID and checks liveness through `/proc`, so a container-private PID namespace leaves it unable to tell when an agent exits. `--pid=host` makes container PIDs and host PIDs the same.
3. **Forward the tmux pane into each exec.** tmux sets `TMUX_PANE` in every pane. Pass it through so the hook can tell the daemon which host pane the agent runs in; jump, send, kill, and the other CLI actions all target that pane.

```jsonc
// .devcontainer/devcontainer.json
{
  "runArgs": ["--pid=host"],
  "mounts": [
    "source=/run/user/1000/atm,target=/run/atm,type=bind"
  ],
  "containerEnv": {
    "ATM_SOCKET": "/run/atm/atm.sock"
  },
  // Mount the workspace at its host path so project grouping, which the
  // host daemon resolves with git, sees the same directory.
  "workspaceMount": "source=${localWorkspaceFolder},target=${localWorkspaceFolder},type=bind",
  "workspaceFolder": "${localWorkspaceFolder}"
}
```

```bash
# From any host tmux pane; each pane forwards its own id
docker exec -it -e TMUX_PANE="$TMUX_PANE" <container> claude
devcontainer exec --workspace-folder . --remote-env TMUX_PANE="$TMUX_PANE" -- claude
```

Inside the container the hook needs `jq` and either `socat` or `nc`, and the hook script must exist at the path recorded in Claude Code's settings (`~/.local/bin/atm-hook` by default). If the container home differs from the host home, run `atm setup` inside the container once. Set `ATM_DEBUG=1` in the container to log hook activity to `/tmp/atm-hook.log` when sessions do not appear.

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
