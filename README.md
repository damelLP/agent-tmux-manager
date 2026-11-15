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

An open TUI restarts the daemon within a few seconds if it goes away, including after `atmd stop`. Quit every TUI before you stop the daemon for good, or before you restart it by hand: a TUI may start it first, with the TUI's environment. The daemon log defaults to `~/.local/state/atm/atm.log` (or `$XDG_STATE_HOME/atm/atm.log` when `XDG_STATE_HOME` is absolute); each background start keeps the previous run's log as `atm.log.1`. Set `ATM_STATE_DIR` to a nonempty directory path to keep the daemon's PID file (`atmd.pid`), logs (`atm.log`, `atm.log.1`) and event log (`log/`), and the TUI's `tui.log`, there instead. Use a directory that survives a reboot: atmd replays the event log at start.

`atmd stop` waits for the daemon's real process exit, not just removal of its socket or PID file. After 5 seconds it prints one progress line and continues waiting, up to 75 seconds from signaling. A nominal shutdown can take 66 seconds: connections (2); then reserved work (agent lifecycle flows, observations, message deliveries) is stopped at once and only its cleanups go on, while messages, agents and control stop in that order, by 20, 22 and 62 seconds from that moment, the last covering admitted control input under its original deadlines (30) and audits (30); then concurrent summary/raw flushes (2). A stage that passes its bound is logged and abandoned, not waited for. A clear that the stop interrupts leaves its agent stuck, with what the clear last waited for. Blocking I/O, including work that Tokio waits for at runtime shutdown, can take longer without a finite bound. On timeout, stop exits non-zero and names the PID that may still be shutting down; it neither force-kills the daemon nor removes its state. Successful stop confirms exit, not that every pending write was persisted.

Known limit (unverified in a seccomp-filtered runtime): if a filter rejects Linux `pidfd_open` with `EPERM`, `atmd stop` fails before signaling the daemon. Only `ENOSYS` enables the old-kernel numeric-PID fallback; a seccomp denial does not.

## CLI at a glance

```bash
atm spawn -m opus -d right         # spawn default harness with model and direction
atm spawn --harness pi             # spawn a specific harness (pi, codex, ...)
pane=$(atm spawn -H pi -w ../wt)   # it prints the new pane's id (-f json: pane, harness, cwd)
atm spawn -H claude -w ../wt --prompt '/atm-implement /tmp/brief.md'  # start on a first prompt (claude, pi, codex)
ATM_SPAWN_PI_BIN=mise ATM_SPAWN_PI_ARGS='x pi' atm spawn --harness pi
```

`atm setup` writes a default `config.toml` once it detects a supported agent, and prints its path (usually `~/.config/atm/config.toml`). It never overwrites an existing file. `atm spawn` and `atm workspace create` only read it, and use built-in defaults when it is missing; a config they can't use (a TOML error, or an unknown default when no `--harness` is given) fails them before they create anything. Default spawn harness (also what `atm workspace create` launches) and per-harness spawn defaults can be configured there:

```toml
[harness]
default = "pi"

[harness.pi]
binary = "mise"
default_args = ["x", "pi"]
```

Environment overrides still take precedence when set (`ATM_SPAWN_PI_BIN/ARGS`, then legacy `ATM_SPAWN_BIN/ARGS`); otherwise config values are used, then built-in defaults.

Codex launches through atm require codex-cli 0.158.0 or later for `--no-daemon` support.

A `BIN` override drops ordinary configured `default_args`, but keeps required arguments (Codex's `--no-daemon`) and arguments from atm's agent roles. The command order is: binary, defaults (unless `BIN` is set), raw `ARGS`, required arguments, role arguments, model, then prompt. For example, `ATM_SPAWN_CODEX_BIN=mise ATM_SPAWN_CODEX_ARGS='x codex'` produces `mise x codex '--no-daemon'`.

If `default_args` or an agent role already holds a required argument, atm types it once, in the required position. Other repeated arguments keep their order and repetitions. Don't use a required argument as another option's value: atm treats an exact match as the required argument, not as a value.

`ATM_SPAWN_<HARNESS>_ARGS` and `ATM_SPAWN_ARGS` remain **caller-owned shell syntax**, passed through verbatim except for trimming whitespace at the ends. ATM does not parse, reject or remove repetitions from raw arguments, even if they repeat or neutralize a required argument. Shell expansions and wrappers can also repeat or neutralize it. Remove any raw `--no-daemon` you previously supplied to avoid repeating ATM's flag, and ensure wrappers forward the appended requirements. Quoting, option values and shell operators in raw `ARGS` are your responsibility.

`atm spawn --prompt` puts the prompt last, after these arguments and `--model`. Don't end them with an option that takes several values, such as Claude Code's `--add-dir` or Codex's `-i`: it would take the prompt as one more value. Don't end them with `;`, `&`, `|` or `#` either: the shell would run the prompt as a command of its own, or read it as a comment.

```
atm role coordinator               # tag your own pane ($TMUX_PANE)
atm role coordinator -t %12         # tag another pane (or a session id prefix)
atm role --clear                    # remove your pane's tag (-t also works)
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

`atm role` stores the tag on the pane, so it survives a clear or harness restart there. With no target or a `%N` target it needs only tmux, not atmd; without `$TMUX_PANE`, pass `-t`. A session-id target uses the daemon. `atm list -f json` adds `role: "coordinator"` when a session's pane is tagged and its parent does not share that pane, or `role: null` otherwise (also on failed tmux lookups). No parent, or a parent missing from the full session list, passes: a separate-process teammate on its own tagged pane can coordinate, but an in-process subagent sharing its parent's pane stays null, even when filters hide the parent. Table and ids formats are unchanged. `/atm-coordinate` tags its pane during setup. The TUI shows a bold cyan `C` on coordinator rows and puts them first within each agent group, keeping newest-first order within each band. Nested teammates stay in place. Press `c` to toggle the selected row's pane; group rows, rows without a pane and children sharing their parent's pane are unchanged. External tag changes appear on the next three-second poll; `c` updates the display immediately.

### Starting a named agent

```bash
atm agent up worker                          # start the agent of role "worker" in this project
atm agent up worker --project-root ../other  # ... in another project
atm agent clear worker                      # clear the agent's current context
atm agent respawn worker                    # replace its pane and start it again
atm agent down worker --kill                # stop the agent and kill its pane
```

For `clear`, `respawn`, `down`, `msg send --to` and `inbox --for`, a bare name resolves in the caller's bound pane's project, or the current directory's project if unbound; `--project-root` selects a project explicitly, and `project/name` addresses another project's agent directly.

`atm agent up <name>` asks `atmd` to start the agent that `.atm/roles/<name>.md` under the project root describes. The project root is the current directory unless `--project-root` is given. `atmd` splits a new pane at 50% without moving your focus, waits there for an idle shell, types one launch line and Enter, and waits for the harness. The line is `cd <the role's cwd> && ATM_AGENT=<project>/<name>`, then the role's `command_prefix`, then the harness command with the role's `spawn_args` and `model`.

- **Where the pane goes.** Below the pane you ran the command from. Run from outside tmux, where there is no such pane, it is below the first pane tmux lists that is the active pane of its window. A role can say `pane = "<left|right|above|below>:<agent name>"` to split on that side of another agent of the same project; that agent must have a pane. Any other form of `pane` is refused before any pane or agent record is changed (the project itself is registered first: `.atm/project.toml` is written the first time a project is used).
- **It does not read `[harness.<name>]` in `config.toml`.** The binary and its default arguments are atm's built-in ones for the role's harness. `atm spawn` reads that table; `atm agent up` does not.
- **`ATM_SPAWN_<HARNESS>_BIN/ARGS` and `ATM_SPAWN_BIN/ARGS` come from the environment `atmd` was started in**, read once at its start. Setting them in the shell where you run `atm agent up` has no effect; restart `atmd` to change them.
- **A `command_prefix` takes the binary out of command position.** Without a prefix the binary is the command word of the line, so a shell alias for it still expands. Behind a prefix (`command_prefix = ["mise", "x", "--"]`) the shell expands no alias: a harness that is installed only as an alias will not launch with a prefix.
- **Printable ASCII only.** If the role's cwd, its model, an argument, a prefix word or an `ATM_SPAWN_*` override holds anything else, the start is refused before any pane or agent record is changed.
- **Leave the new pane alone until the launch line has run.** `atmd` reads the shell's prompt once (what is on the line before the cursor), on a pane tmux shows no client focused on, and types only after exactly that prompt. If you select the new pane before the prompt was read, or tmux cannot tell `atmd` whether you did, the start ends as `stuck` with nothing typed: you may have typed there, and atm cannot tell your text from the prompt. Start the harness in that pane yourself and adopt it, or take the agent down and up again. If you select the pane after the prompt was read, `atmd` waits until you leave it. Before the launch line is typed, a line that then reads anything but that prompt ends the start as `stuck` with nothing typed. Once it is typed, `atmd` waits, and presses Enter only when the line reads exactly the prompt followed by the launch line again; if it never does, the start ends as `stuck` at its deadline with the line typed and not run. Two things `atmd` cannot see, and what you typed then runs together with the launch line: a selection that falls between two of its looks at the pane (they are a quarter of a second apart, or more when the machine is busy), and text left behind the cursor (typed, then Home) at any moment before the Enter.
- **Prompts it cannot launch into, or launches into too early.** A prompt that redraws itself after it was read, or carries a clock, ends the start as `stuck`: with nothing typed if it changes before the launch line is typed, with the line typed and not run if it changes after. A prompt whose last line is empty, and a pane too small for the launch line once it wraps, end the start as `stuck` too (nothing typed, or the line typed and not run and left at the prompt for you). Startup output that ends without a newline (`Loading...`) can be taken for the prompt: the launch line is then typed after it, before the shell's own prompt, and usually runs when the shell gets to it.
- **A start has 60 seconds** (the command itself waits up to 85). Once `atmd` knows the start is recorded, any failure, and the deadline, leave the agent `stuck` with the reason `start did not complete: <why>; inspect %N` and one note in your inbox. When the deadline ends it, the command fails with that same reason, not with a bare timeout. `atm agent clear` at its `--timeout-ms` and `atm agent respawn` at its 60 seconds answer the same way: the command fails with the reason the agent is then `stuck` with (`clear did not complete: <why>; inspect %N` for a clear), not with a bare timeout. Two cases end otherwise: if the answer to recording the start is lost, there is no `stuck` and no note, and the agent goes `down` at the next check of its pane; and if `atmd` cannot read its log or write the `stuck` record when the start ends, the agent stays `starting` and takes no further command until `atmd` is restarted. A pane the start created stays for you to look at. If the start failed before its pane was recorded, the agent then goes `down` and its reason keeps that text.
- **Start dialogs.** `atmd` answers two questions a harness may ask at its start, each key only while the pane still shows that same question and no tmux client is focused on it. A folder trust dialog (Claude Code's, Codex's "Folder access") is accepted only when its folder is exactly one of the role's `trust_dirs` (resolved against the project root); any other folder ends the start as stuck with no key sent. Codex's update offer is skipped with one Escape. Every other question (Codex's "Hooks need review", a startup warning, a permission dialog) is left for you: the start ends as stuck, or waits until its 60 seconds are over.

### Sending text

`atm send` refuses input when the window has `synchronize-panes` on (Enter could
reach sibling panes), when the pane has input off (tmux would silently drop it),
or when the text contains ASCII control bytes other than tab, LF and CR (including
ESC, NUL and DEL). These refusals send nothing. Newlines remain allowed unless
bracketed paste is explicitly off; the existing copy-mode and dialog checks also
apply.

## Confirmed messages and inbox

The message CLI is distinct from the immediate `atm send` command. It calls the daemon's `msg.*` service, with no direct-tmux fallback. `atmd` starts the runtime at startup using the configured delivery policy or its defaults.

```sh
atm msg send --to worker --kind brief --body 'Read the attached brief' --attach /absolute/path/brief.md --task task-1
atm msg send --to owner --kind report --body 'Tests passed'
atm inbox                             # your inbox; does not mark anything read
atm inbox --for worker --unread --limit 100
atm inbox --for owner --unread --limit 100
atm inbox --id m123 --format json      # full original body, including newlines
atm inbox --after m123 --limit 100      # explicit pagination; follow next_after
atm msg ack m123 m124                  # only your delivered messages
atm msg retry m123 --accept-duplicate-risk
atm msg give-up m123
```

All commands accept `--format text|json` (default `text`). Send requires `--to` and `--body`; optional flags are `--kind`, repeatable `--attach`, `--task`, and `--project-root`. Kinds are `note` (default), `brief`, `report`, `question`, `answer`, and `review`. Inbox accepts `--for`, `--project-root`, `--unread`, `--id`, `--after`, and `--limit`; `--id` and `--after` are mutually exclusive. IDs use canonical `mN` form.

- Targets are `owner`, a full `project/name` address, or an exact bare agent name. Full addresses bypass name lookup. Bare names use `--project-root` if supplied (relative paths become absolute); otherwise they use the uniquely bound, non-Down caller pane's project, or the absolute current directory if unbound. Unknown/ambiguous names or caller bindings fail rather than guessing across projects. `system` is not a recipient or inbox.
- Sender attribution comes from the caller's pane binding; an unbound caller is Owner. There is no `--from` or send-now bypass. Owner can inspect agent inboxes; agents can inspect only their own. System notices and Owner-only resolution rely on local socket/client-supplied pane attribution, **not authentication against hostile local clients**.
- Body limit: 16 KiB of UTF-8 bytes, with no trimming. An empty body needs at least one attachment. At most 32 attachments, each a nonempty literal path of at most 4096 bytes; task IDs are at most 256 bytes. Prefer absolute attachment paths across worktrees. Neither CLI nor daemon uploads/reads attached files, checks their existence, or automatically reads stdin. Quote multiline shell arguments to preserve blank lines and Unicode.
- Every agent delivery uses the one-line reference defined by `message_delivery::inbox_reference`: for `m123`, `[m123] Read: atm inbox --id m123`, with no trailing newline. The body, task and attachments stay intact in the inbox. **Delivered means the harness accepted the retrieval reference**, not that it read the body or did the work. Owner messages are delivered directly to the inbox without terminal input.
- Inbox includes full bodies, metadata, durable status and projected hold/retry/predecessor fields. Text shows a `delivery: <phase>` line with pane, attempt, blocking predecessor (`blocked_by`), retry time (`retry_at`) and reason only when present; JSON retains the complete delivery diagnostics. Text indents every body line by two spaces under its entry, preserving body backslashes, line breaks and tabs and escaping other control characters. Prefer `--id` for individual reads; `--format json` is the exact form, preserving original content, nulls and additive daemon fields (DEL/C1 are JSON Unicode escapes). Limit is 1–1000 (default 100), even with `--id`; pages are bounded to 512 KiB without truncating bodies. An oversized legacy entry is refused with its ID. `next_after` indicates more matching messages. Unread counts cover all statuses, independent of page/filter.
- Fetching never acknowledges reading. Explicit `atm msg ack` takes 1–1000 distinct IDs from your inbox; the whole batch must exist and be Delivered. A wrong recipient or undelivered entry refuses the whole batch. Already-read IDs are idempotent; output counts only newly marked messages. If fetch precedes the delivery receipt, wait for Delivered before acknowledging, not by sending again.
- Retry is Owner-only for a latched/held delivery and **keeps the same ID and increasing attempt count**. Inspect the pane first: retry can duplicate delivery, hence the required `--accept-duplicate-risk`. It does not reset the attempt limit or bypass safety guards. After another restart an unconfirmed attempted message is held again. Give-up is allowed for held Queued/Delivering messages, not just one Blocked reason; repeating it on Failed is idempotent. A late receipt can still change a given-up message to Delivered, and cancels a pending same-ID retry. Already Delivered messages cannot be retried or given up.
- A timeout is outcome-unknown, not rollback. Inspect inbox/pane before deciding what to do; the CLI never automatically resubmits. Ordinary message calls allow 15 seconds (10-second router budget + 5 margin); retry/give-up allow 70 seconds (60 + 10); name lookup allows 20 seconds per `agent.list` call (10 + 10).

`atm status` requests only Owner's aggregate unread count, even from an agent pane: it appends `N unread` when nonzero and leaves the existing summary unchanged at zero. An old/unavailable daemon or invalid aggregate response produces `? unread` plus a stderr diagnostic, not a false zero or any Owner bodies. Command errors exit 1; invalid command syntax exits 2.

### Delivery configuration

`atmd` reads `[delivery]` and `[notify]` from the same bounded startup file read. Missing file/section uses the delivery defaults; unknown delivery keys, invalid values, malformed TOML or a failed/timed-out read return an explicit policy error, never a valid-looking default. A bad delivery section does not disable a valid notification section (nor vice versa). Restart to pick up changes. If the delivery policy is invalid or the runtime cannot start, message commands (including `atm inbox`) return `message service unavailable` with the cause, which is also logged by the daemon.

Durations use `{ secs, nanos }`, not strings or bare numbers. Omitted fields take these defaults:

```toml
[delivery]
stable_idle = { secs = 10, nanos = 0 }
stable_empty_input = { secs = 5, nanos = 0 }
confirmation_timeout = { secs = 3, nanos = 0 }
retry_delays = [{ secs = 1, nanos = 0 }, { secs = 2, nanos = 0 }, { secs = 4, nanos = 0 }]
stale_after = { secs = 120, nanos = 0 }
readiness_timeout = { secs = 120, nanos = 0 }
guard_poll_interval = { secs = 0, nanos = 250000000 }
pane_gone_grace = { secs = 2, nanos = 0 }
```

Durations must be positive and fit the policy's checked timestamp arithmetic; retry delays must be nondecreasing. Maximum attempts are the number of retry delays plus one (four by default); an empty retry list permits one attempt. Timing defaults do not prove a harness editor is safe: observations and safety guards still control delivery.

## Notifications

`atmd` can run a command of yours whenever a session starts needing input: a permission dialog, an MCP input dialog, or an agent asking you something, in any supported harness. The same command also handles newly committed Owner inbox messages, under the separate rules below. Add a `[notify]` section to `config.toml`. `atm setup` writes this example commented out, but it never rewrites an existing file, so add the section by hand if you already have one:

```toml
[notify]
command = ["notify-send", "atm", "--"]
```

`atmd` appends one message as the last argument, such as `agent-tmux-manager/main %5: claude needs input (Bash: cargo test)` (project and branch, pane, harness, and what it asks; a subagent's row says `claude subagent`), and runs the command directly. No shell parses the message. Keep the `--` for a command that takes options, so a message that starts with `-` isn't read as one. The message names the tool and a short summary of its input, such as the start of a shell command, so a script that sends it off your machine (Slack, ntfy) can trim it first.

- It runs once each time a session changes into needing input. Nothing runs while a session stays there, when a turn ends, or for Claude's "waiting for your input" reminder (`idle_prompt`), and the sessions `atmd` finds at startup never set it off. A subagent's dialog that stays open for more than about 6 seconds also sends a second message for its lead, because Claude marks the lead as waiting too.
- `delay_secs = N` (default `0`, at once) waits N seconds first, and sends nothing if the dialog was answered by then; a new dialog starts its own wait. Claude Code waits about 6 seconds before its own reminder for an open dialog, so around `10` reads as "I'm away from the desk". Claude Code sends no hook when you approve a dialog, so the session keeps reading as needing input (in the TUI too) until the approved tool finishes: approving a command that runs longer than the delay still sends a message.
- `atmd` reads `[notify]` when it starts. Run `atmd stop` to apply a change: an open TUI starts it again, or the next `atm` does. An invalid `config.toml`, or an unknown key in `[notify]`, is logged to the daemon log, and `atmd` starts without notifications.
- The command runs in `atmd`'s environment, which comes from whoever started `atmd` (usually a TUI). Use absolute paths: with no shell, `~` and `$VAR` aren't expanded.
- Stdin and stdout are closed. After 30 seconds `atmd` kills the command (but not processes it started). A failure is logged, with the command's stderr, to the daemon log.

For Owner inbox messages, a separate consumer subscribes to committed events before service admission, skips startup history and deduplicates replay overlap within the process. It notifies only new Owner messages; OwnerFocused notices remain **inbox-only** (no `[notify]`). Its notification contains bounded metadata and an inbox reference with `--for owner`, not the body. `delay_secs` delays launch, and committed read state is checked again just before launch: reading while queued suppresses it, as does an unavailable state read. It has one running/delayed command and at most 32 pending commands; overflow is logged without changing the durable inbox. Commands use the same no-shell runner and 30-second timeout as attention notifications, which retain their independent per-session behavior. Notification failure never generates another message or retries the command. This external effect is **best effort**, not durable/exactly-once: shutdown cancels pending work and a crash can lose a notification. A watch error stops Owner notifications until restart and logs one warning; consult the inbox/status for durable state.

[ntfy](https://ntfy.sh), through curl. `--data-raw` sends the message as it is; `-d` would read a file for a message that starts with `@`:

```toml
[notify]
command = ["curl", "-fsS", "https://ntfy.sh/<your-topic>", "--data-raw"]
```

A Slack incoming webhook, through a small script that JSON-escapes the message:

```sh
#!/bin/sh
# Posts $1 to the Slack incoming webhook whose URL is in the file below.
url=$(cat "$HOME/.config/atm/slack-webhook-url") || exit 1
jq -n --arg text "$1" '{text: $text}' |
  curl -fsS -X POST -H 'Content-Type: application/json' --data @- "$url"
```

```toml
[notify]
command = ["/home/you/bin/atm-slack"]
```

## How it works

```
Claude Code / pi / Codex  ──hook/extension──▶  atmd (daemon)  ◀──socket──  atm (TUI/CLI)
```

`atm setup` registers supported harness integrations (Claude Code hooks, the pi extension, and Codex CLI hooks). Harness events are forwarded to the `atmd` daemon over a Unix socket, and `atm` connects for real-time display. Codex context usage is read on a best-effort basis from the rollout transcript supplied with hook events; a missing or changed transcript never blocks lifecycle updates. Codex requires a one-time trust approval of the installed hooks: run `/hooks` inside codex after `atm setup`.

For Claude Code, `atm setup` adds its `statusLine` only when none exists. It keeps a user's existing statusLine unchanged and writes no backup. ATM reads Claude Code context, model and cost only through `atm-hook`, so your status command should pass its stdin JSON to that hook. Setup prints an example using the installed hook's path; inside a wrapper, buffer stdin and forward it, then reuse `$input` for your own rendering:

```sh
input=$(cat)
hook="$HOME/.local/bin/atm-hook"
if [ -x "$hook" ]; then printf '%s' "$input" | "$hook"; fi
# Render your status line from "$input" here.
```

To use ATM's statusLine instead, remove your `statusLine` entry from `~/.claude/settings.json` and run `atm setup` again. `atm uninstall` removes or restores over only an exact ATM-generated entry: the two fields `"type": "command"` and `"command"` set to the raw absolute path of the current home's `~/.local/bin/atm-hook`, or the historical `/usr/local/bin/atm-hook` fallback, with no extra fields. It keeps every other statusLine, including wrappers and inline commands that call `atm-hook`, and prints a notice. A backup saved by an older version is restored only over ATM's entry; when your own statusLine is present, a readable stale backup is discarded.

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

## Coordinating agents (experimental)

`atm setup` installs two skills for Claude Code, pi and Codex: `atm-coordinate` lets one agent drive long-lived implementer agents through `atm list`, `peek`, `send` and `reply`, and `atm-implement` gives each implementer its rules. Invoke them as `/atm-coordinate` (Claude Code), `/skill:atm-coordinate` (pi) or `$atm-coordinate` (Codex). Their source, install layout and usage are in [`crates/atm/assets/atm-skills`](crates/atm/assets/atm-skills/README.md).

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
