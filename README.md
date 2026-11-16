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
atm task list --open               # unfinished tasks; get t41 shows every field
atm decisions                      # pending/provisional decisions for you
atm decide d41 yes 'Approved'      # owner answer; --rule appends a standing rule
atm away                           # you will not answer: agents go on under the away rules
atm back                           # you are back: nothing more is decided for you
atm status                         # one-line summary for tmux status bar

atm workspace create               # new session with ATM sidebar + agent + shell
atm workspace attach               # inject sidebar into current session
atm layout pair                    # two agents + ATM sidebar
```

Known reply limit: `-y`/`-n` can wait behind reserved work and do not re-check the screen when released, so their keys go to whatever is then visible.

`atm role` stores the tag on the pane, so it survives a clear or harness restart there. With no target or a `%N` target it needs only tmux, not atmd; without `$TMUX_PANE`, pass `-t`. A session-id target uses the daemon. `atm list -f json` adds `role: "coordinator"` when a session's pane is tagged and its parent does not share that pane, or `role: null` otherwise (also on failed tmux lookups). No parent, or a parent missing from the full session list, passes: a separate-process teammate on its own tagged pane can coordinate, but an in-process subagent sharing its parent's pane stays null, even when filters hide the parent. Table and ids formats are unchanged. `/atm-coordinate` tags its pane during setup. The TUI shows a bold cyan `C` on coordinator rows and puts them first within each agent group, keeping newest-first order within each band. Nested teammates stay in place. Press `c` to toggle the selected row's pane; group rows, rows without a pane and children sharing their parent's pane are unchanged. External tag changes appear on the next three-second poll; `c` updates the display immediately.

### Starting a named agent

```bash
atm agent up worker                          # start the agent of role "worker" in this project
atm agent up worker --project-root ../other  # ... in another project
atm agent clear worker                      # clear the agent's current context
atm agent handover worker                   # ask it to save its state, then clear it
atm agent respawn worker                    # replace its pane and start it again
atm agent down worker --kill                # stop the agent and kill its pane
```

For `clear`, `handover`, `respawn`, `down`, `msg send --to` and `inbox --for`, a bare name resolves in the caller's bound pane's project, or the current directory's project if unbound; `--project-root` selects a project explicitly, and `project/name` addresses another project's agent directly.

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
- **The start line.** When `atm agent up`, `atm agent respawn` or `atm agent clear` ends with the agent `ready`, `atmd` queues one note from `system` to the agent and types it into the agent's editor as `[mN] New session: run the shell command atm me`, so that the new session finds its role, memory file and messages by itself. The line names a shell command because a model read the shorter form as a request to run a personal skill. This starts a model turn. It is delivered like any message (see [Confirmed messages and inbox](#confirmed-messages-and-inbox)): only into an empty editor of an idle session, never into a draft or the pane you are focused on, and confirmed by the harness's own prompt event; you see it in `atm inbox --for <agent>`, and `atm me` does not list it. Messages to an agent are typed oldest first, so one queued before the start line is typed before it. A second start line is not queued while one to the agent is still waiting. A start or clear that does not end `ready` queues none, and neither does `atm agent adopt`. If the agent goes `down` or `stuck` before its start line is delivered (a respawn records it `down` for a moment, so it can do this too), the line is blocked: you get a `mN blocked: recipient agent is stuck or down` note and your `[notify]` command runs. Delivery resumes when the agent is `ready` again, and `atm msg give-up mN` cancels it. Set `ATM_SPAWN_START_LINE=off` in the environment `atmd` was started in to turn start lines off.

- **Start-line width.** ATM refuses to submit a wrapped start-line draft in Claude or pi: no Enter, so no automatic delivery confirmation; if the line was pasted, it stays in the editor for inspection. For `m123`, the text occupies 48 columns: synthetic edge-wrap checks fail below 50 pane columns for Claude (including its two-column prompt) and 48 for pi. Each extra ID digit needs another column. These are matcher checks, not verified real-harness wrapping boundaries.

### Handing an agent over

`atm agent handover <agent>` does in one command what a clear by hand needs three steps for. `atmd` sends the agent one message that asks it to write what its next session needs to `.atm/memory/<name>.md` in the project's main checkout. It waits until that turn has ended and the file has changed, and then clears the agent as `atm agent clear` does. The new session finds the file through `atm me`.

```bash
atm agent handover worker                      # waits up to 10 minutes for the save, then clears
atm agent handover worker --timeout-ms 120000  # 30000 to 1800000
atm agent handover worker --format json        # {"agent_id":…,"message_id":"m12","state":"cleared","session_id":…}
```

- **Nothing is cleared unless the agent saved.** `atmd` notes the memory file's size and modification time when the command starts, just before it queues the request. Saved means that, once the save turn has ended, the file is there and either differs from that or is new. A change made before the request reaches the agent counts too, for example by an agent that writes its file and asks for its own handover in the same turn. The content is not read. If the save turn ends and the file is as it was, or it has been removed, the command fails with `<agent> ended its save turn and <file> did not change; nothing was cleared`.
- **The request is an ordinary message** from `system`, typed as `[mN] Read: atm inbox --id mN` under the usual delivery rules: an idle agent, an empty editor, no dialog, and not the pane you are focused on. If it is not delivered within the timeout the command fails and says so. The request stays queued, the next `atm agent handover` of that agent reuses it, and `atm msg give-up mN` cancels it.
- **If the save turn does not end in time** the command fails with `<agent>'s save turn did not end in <n> s`, and adds `it waits at a dialog in %N` when the agent is waiting for an answer, for example a permission to write the file.
- **If the clear is refused after the save** (you are focused on the pane, a draft is in the editor, a new turn has started) the command fails with `<agent> saved its state (mN); ` followed by the clear's own reason. Run `atm agent clear <agent>` when the pane is free.
- **An agent may hand itself over.** From its own pane the command answers `started` at once, because the request can only arrive after the agent's current turn has ended. The agent then ends its turn, is asked to save, and is cleared. If that handover fails, the agent's manager (you, when its role names none) gets one note with the reason. The same note is sent when another caller's command is gone before the handover fails, for example a command that was killed while it waited: the handover carries on, and its failure goes to the agent's manager instead. From an agent's shell, whose tools may stop a long command, run a handover in the background.
- **Only a `ready` agent with a known session** can be handed over, and one handover per agent runs at a time. Anyone may ask, as for messages.
- **A restart of `atmd` during a handover** loses it: the request is still delivered and the agent saves, but nothing is cleared. Run the command again.

**A note at the context line.** When a named agent's turn ends and its context is at 60% or more, `atmd` sends one note to the agent's manager (to you for an agent whose role names none, and for an adopted agent): `agent <id> is at <n>% of its context (line 60%). Hand it over: atm agent handover <id>`. It is sent once per session and nothing is cleared. After a restart of `atmd` a session that was already noted can be noted once more. The line is 60% for every harness and model: a chosen number, not a measured one, until measurements give better ones.

**Handover without a command.** A role file can let `atmd` hand its seat over by itself. It is off unless the role says so:

```toml
auto_handover = ["context"]   # or ["context", "idle"]; leave it out for off
context_line  = 60            # optional: this seat's line in percent, 1 to 100 (default 60)
cache_minutes = 60            # optional, required for "idle": how long you assume the prompt cache lives
idle_minutes  = 48            # optional: default four fifths of cache_minutes, and always below it
```

- **`"context"`:** when a turn of the seat ends at or above its line, `atmd` runs the handover above itself: the same save request, the same wait for the memory file, the same clear. The seat's manager gets one note when it is over, `automatic handover of <id> done (context 63%, line 60%): it saved to <file> (mN) and has a new session.` or `automatic handover of <id> (…) did not complete: <reason>. Nothing more is tried in this session. By hand: atm agent handover <id>`, in place of the note at the line. `context_line` also moves that note for a seat that is off.
- **`"idle"`:** when the seat has been idle for `idle_minutes` and nothing it waits for will come before `cache_minutes` are over, it is asked to save while its cache is still warm, and the handover gives up when they are over. `atmd` has no cache life of its own, because none has been measured: without `cache_minutes` there is no idle rule. What a seat waits for is read from tasks and decisions only: a task it holds or has given to another agent, or a pending decision it asked for (your answer counts as coming in time). A seat that works by messages alone waits for nothing as far as `atmd` can see, so use `"idle"` only where tasks carry the work.
- **The file is read at every turn end,** in the project's main checkout, as `atm me` reads it. An edit takes effect at the seat's next turn end with no restart and no respawn, and removing `auto_handover` stops it. A missing or invalid role file is off.
- **It never does more than the command would.** Nothing is typed while a turn runs, over a draft, at a dialog or into the pane you are focused on: the request waits there as any message does, and after ten minutes the manager is told. Nothing is cleared unless the agent saved. A session that shows background work is left alone, and so is one that has done nothing but start.
- **One try per session.** Whatever becomes of it, `atmd` asks no second time in that session; the next session has a try of its own. If the start-line turn already ends at or above its line, the note says that this session will not be handed over automatically: neither the context nor the idle rule runs in it, even after later work. A handover that could not start (the agent is not ready, another handover of it runs) sends nothing and does not count.
- **A session whose cache has run out is not cleared.** With `"idle"` on, a seat idle for longer than `cache_minutes` is left as it is, and gets the note at its line. Clear it with a command.
- **Limits.** All of it is in memory: after a restart of `atmd` nothing is done until a turn ends, and a handover the restart cut off is lost with its note. A request that was never delivered when the handover gave up stays queued, as for the command: once what held it is gone it is typed, the agent saves, and nothing clears the session. For an idle seat that late turn runs on a cold cache; `atm msg give-up mN` cancels the request. An older `atmd` refuses a role file that has these keys (`agent up` fails and `atm me` reports an invalid role), so remove them before going back to one.
  - Admission does not re-check the session the watcher judged: a clear/rebind in that tiny window can target the new session (reasoned, unverified).
  - After a daemon restart, a session with a recorded start line has its first turn end counted as "not worked"; at the line this excludes it from automatic handover for that session.
  - Even with all automatic rules off, every registry session's turn end copies the full organisation state; a Ready seat also reads its role file, and its line note waits for background work to end.

### Finding your instructions and handover

From a named agent's pane, start with one command:

```sh
atm me                         # identity, manager, instructions and what is waiting
atm me --format json           # the same context as structured data
```

`atm me` reads the current `.atm/roles/<name>.md` in the project's main checkout on every call. It shows your full agent id and manager, the role's instructions, unread messages (id, kind, sender and first line; the start line that pointed you here is left out), the absolute `.atm/memory/<name>.md` path and whether it exists, and unfinished tasks assigned to you (id, title, kind, state, agent and reason). Read an existing memory file for handover. The caller's pane binding determines identity, not the working directory or `ATM_AGENT`; an unbound caller gets a clear refusal.

Every unread message includes `atm inbox --id mN` to retrieve its full body and attachments. Text adds `After handling a delivered message: atm msg ack <id>` after a nonempty unread list (replace `<id>` with the message id). Fetching context does **not** mark messages read: use `atm msg ack mN` explicitly after reading a Delivered message. Unread summaries include queued and failed messages too, in message sequence order.

The last line is the exact reporting template for your manager, for example:

```sh
atm msg send --to project/lead --kind report --body 'DONE: <result>'
# Replace the body; optionally add --attach /absolute/path/report.md.
```

A missing role file is shown explicitly with `instructions: null` in JSON; the other context remains available and reports go to `owner`. A role with no manager also reports to `owner`. A bare manager name resolves in your project; a full `project/name` reaches another project. An invalid role or manager is refused, never replaced by cached instructions. Unexpected file errors are reported, not treated as missing. Memory is checked but never read or created by this command.

Role files are limited to 256 KiB and the context result to 512 KiB, without silently truncating instructions or summaries. An oversized context is refused; use `atm inbox --unread` for paged messages. Each caller has five seconds total to wait for the file-read slot and its result. Overlapping calls wait without starting extra threads; at most one detached read is in flight. A stalled read times out its callers without blocking daemon shutdown or accumulating workers. The CLI allows 17 seconds including startup and transport. JSON preserves original content and additive daemon fields; text escapes terminal controls.

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
- Every agent delivery uses the one-line reference defined by `message_delivery::inbox_reference`: for `m123`, `[m123] Read: atm inbox --id m123`, with no trailing newline. The one exception is the daemon's start line (see [Starting a named agent](#starting-a-named-agent)), typed as its own text, `[m123] New session: run the shell command atm me`. The body, task and attachments stay intact in the inbox. **Delivered means the harness accepted the retrieval reference**, not that it read the body or did the work. Owner messages are delivered directly to the inbox without terminal input.
- Inbox includes full bodies, metadata, durable status and projected hold/retry/predecessor fields. Text shows a `delivery: <phase>` line with pane, attempt, blocking predecessor (`blocked_by`), retry time (`retry_at`) and reason only when present; JSON retains the complete delivery diagnostics. Text indents every body line by two spaces under its entry, preserving body backslashes, line breaks and tabs and escaping other control characters. Prefer `--id` for individual reads; `--format json` is the exact form, preserving original content, nulls and additive daemon fields (DEL/C1 are JSON Unicode escapes). Limit is 1–1000 (default 100), even with `--id`; pages are bounded to 512 KiB without truncating bodies. An oversized legacy entry is refused with its ID. `next_after` indicates more matching messages. Unread counts cover all statuses, independent of page/filter.
- Fetching never acknowledges reading. Explicit `atm msg ack` takes 1–1000 distinct IDs from your inbox; the whole batch must exist and be Delivered. A wrong recipient or undelivered entry refuses the whole batch. Already-read IDs are idempotent; output counts only newly marked messages. If fetch precedes the delivery receipt, wait for Delivered before acknowledging, not by sending again.
- Retry is Owner-only for a latched/held delivery and **keeps the same ID and increasing attempt count**. Inspect the pane first: retry can duplicate delivery, hence the required `--accept-duplicate-risk`. It does not reset the attempt limit or bypass safety guards. After another restart an unconfirmed attempted message is held again. Give-up is allowed for held Queued/Delivering messages, not just one Blocked reason; repeating it on Failed is idempotent. A late receipt can still change a given-up message to Delivered, and cancels a pending same-ID retry. Already Delivered messages cannot be retried or given up.
- A name-lookup timeout means no operation was sent. Once an operation request is sent, a timeout is outcome-unknown, not rollback. Inspect inbox/pane before deciding what to do; the CLI never automatically resubmits. Ordinary message calls allow 15 seconds (10-second router budget + 5 margin); retry/give-up allow 70 seconds (60 + 10); name lookup allows 20 seconds per `agent.list` call (10 + 10).

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

## Task budgets

A task's optional `budget` on `task.create` accepts exact cost (`{"microdollars":1000000}`), turns and an RFC3339 UTC deadline. Cost and turns cross strictly **above** their limits; a deadline is due **at** its timestamp. Unavailable usage is unknown, not zero. A past deadline is valid; a newly requested negative cost is refused.

A crossed limit sends the creator one ordinary system note **per task**, across all its limits and state changes. A system-created task notifies the owner. The committed message is the latch even when queued, read or failed, so restarts do not create duplicate notes. `task.get` includes `budget_status` with current crossed limits and the notification id. Deadlines include open, waiting and review time; accepted/cancelled tasks are evaluated through their completion time, not a later restart time. **Nothing is stopped, cancelled or blocked.** Notifications need a valid delivery configuration; after fixing it and restarting, budget checks catch up. With valid delivery, `[reminder] enabled=false` disables reminders only, never budget deadlines.

## Task usage

`atm task get <id>` (or the `task.get` RPC) includes `usage`: observed prompt turns, wall-clock seconds in each task state, review rounds, changes-requested verdicts, cumulative-cost deltas, and peak reported context percentage. The same totals are readable while work runs. Final result acceptance persists a frozen copy in `task.cost`; approving a plan instead resumes work. Records and running baselines survive daemon restart, and a missing cost append after an accepted verdict is repaired on startup.

Only a proven parent session bound to a named agent can contribute. Exactly one open task must be assigned to that agent; otherwise measurements are unattributed. A first cumulative price establishes a baseline, not a charge for historical work; later positive high-water increases count once. Attribution follows observation order, so delayed reports can cross task boundaries. Counts reflect observations, not a complete billing history: lost/overflowed measurements and time spent on an older daemon cannot be recovered. Working/idle edges, tool calls and provider replies are not additional user turns.

Missing cost/context is `null`, not reported zero. Codex reports no USD price; context requires explicit active tokens and window capacity, never a guessed model window or lifetime tokens. Peak percentages round up to whole percent and survive compaction. State time includes waiting and daemon downtime, stops at acceptance/cancellation, and sums subsecond visits before reporting whole seconds. Review starts (or direct verdicts naming a review) count rounds; automatic closure of a review does not create another round. Files/lines changed and files read are not measured here. No agent is stopped by accounting.

## Tasks

`atm task` calls the daemon's nine `task.*` methods, with one request per command and no direct-tmux fallback or automatic retry. Tasks persist in the organisation event log across daemon restarts. A task id is `t` plus its creation event sequence (`t41`, not `t041`); use the id printed by `create` in the examples below.

From the creator's shell:

```sh
atm task create 'Build the change' --measure '{"kind":"mechanical","command":"cargo test --workspace"}' \
  --file '{"path":"brief.md","kind":"brief","note":null}'
atm task assign t41 project/worker
atm task get t41
atm task list --open --agent project/worker
```

From the assigned named agent's pane:

```sh
atm task set-state t41 working
atm task set-state t41 waiting --reason 'Waiting for the owner'
atm task set-state t41 blocked --reason 'Required input is missing'
atm task set-state t41 working
atm task submit t41 --what plan --file '{"path":"plan.md","kind":"plan","note":"Review before building"}'
# After the creator/lead/owner accepts the plan, the task is working again.
atm task submit t41 --file '{"path":"report.md","kind":"report","note":null}'
```

The default submission is `result`. Creator, lead or owner can request changes or accept, subject to the measure; nobody can give a verdict on their own assigned task:

```sh
atm task verdict t41 changes_requested --note 'Add the missing case'
atm task verdict t41 accepted --note 'Checked against the measure'
# A separate review is created with the task it judges, then assigned to a fresh reviewer.
atm task create 'Review the change' --kind review --judges t41 --measure '{"kind":"owner"}'
atm task assign t52 project/reviewer
atm task review t41 t52
# After the reviewer submits t52 as a result:
atm task verdict t41 accepted --review-task t52 --note 'Review passed'
atm task attach t41 evidence.log --kind evidence --note 'Verification output'
```

These are examples of separate paths, not commands to execute unconditionally in order: a task must be submitted to start review or receive its first verdict, and an accepted result is final. An accepted **plan** resumes work instead of finishing the task. A verdict with a submitted review task also accepts that review task. Reviewed measures require such a review for result acceptance unless the owner accepts; owner-judgement results require the owner.

### Task commands and data

Every subcommand accepts `--format text|json` (default text); the option also works before the subcommand, e.g. `atm task --format json get t41`. JSON preserves the complete original daemon result, including additive fields and newer enum strings. Text escapes terminal controls; `get` shows every task field, including the measure, budget, decision reference, files, timestamps and cost record when present. Newer enum values read as `unknown` in summaries rather than making reads fail; full raw get fields and JSON remain visible. This daemon refuses unknown enum inputs.

| Command | Operands and optional flags |
|---|---|
| `create <title>` | Required `--measure <JSON>`; `--kind <kind>` (default `work`), `--parent <id>`, `--judges <id>`, `--budget <JSON>`, repeatable `--file <JSON>`, `--rests-on <decision-id>` |
| `assign <id> <project/name>` | Queues the task's brief to its assigned agent |
| `set-state <id> <state>` | `--reason <text>`; directly settable states are `open`, `working`, `waiting`, `blocked`, `cancelled` |
| `submit <id>` | `--what plan\|result` (default `result`), repeatable `--file <JSON>` |
| `attach <id> <path>` | Required `--kind <file-kind>`; `--note <text>` |
| `review <id> <review-task-id>` | Starts an existing review of the submitted task |
| `verdict <id> <accepted\|changes_requested>` | `--review-task <id>`, `--note <text>` |
| `list` | `--agent <project/name>`, `--creator <owner\|system\|project/name>`, `--state <state>`, `--kind <kind>`, `--parent <id>`, `--after <id>`, `--open`, `--limit <1..1000>` (default 100) |
| `get <id>` | Complete task, not just its list summary |

Agent ids must be full `project/name` addresses for these commands: there is no bare-name lookup, implicit project filter or `--project-root`. `list` includes closed tasks unless `--open` excludes accepted/cancelled tasks. All filters intersect. A `next_after` JSON cursor or `next: --after tN` text line means another matching page exists; repeat the command with that cursor and the same filters. Pages/get are bounded to 512 KiB and are never silently truncated.

Kinds are `work`, `review`, `plan`, `errand`, `drift_check`. The ten states are `open`, `assigned`, `working`, `waiting`, `blocked`, `submitted`, `in_review`, `accepted`, `changes_requested`, `cancelled`. Assign/submit/review/verdict have dedicated commands rather than direct state changes. Waiting and blocked require a reason. Creator/lead/owner can reopen eligible tasks with `set-state ... open` or cancel them with `set-state ... cancelled`; accepted/cancelled tasks are final.

Structured flags use the method's JSON objects, not filenames containing JSON:

```sh
# Choose one measure; no command or rubric is executed/read by this CLI.
--measure '{"kind":"mechanical","command":"check"}'
--measure '{"kind":"reviewed","rubric":"rubric.md"}'
--measure '{"kind":"owner"}'
# Each file has its own kind and optional note; repeat --file for different files.
--file '{"path":"brief.md","kind":"brief","note":null}'
--budget '{"cost":{"microdollars":1000000},"turns":10,"deadline":"2026-12-01T00:00:00Z"}'
```

File kinds are `brief`, `report`, `evidence`, `plan`, `contract`. Budget fields are optional; cost is an integer **microdollar object**, not floating USD, and deadlines use RFC3339. Unknown keys in `--file` and `--budget` objects (including nested cost keys) are syntax errors, refused before contacting the daemon rather than silently discarded. Budget data does not make this CLI stop work. File paths are literal: neither client nor daemon reads/uploads attached files or checks they exist; prefer absolute paths across worktrees. Optional notes default to null. Quote multiline arguments to retain exact text; no stdin is read automatically.

### Task identity and notification outcomes

The daemon derives the caller from its pane, not its working directory or `ATM_AGENT`. A named member may work/wait/block/submit only its assigned task. The task's creator, the named lead and owner can assign/manage/review/give verdicts; a task's assigned agent cannot approve itself, and the measure can further restrict acceptance. An unbound caller counts as owner, just as for messages. There is no `--from`, `--by` or authority bypass; pane attribution is not authentication against hostile local clients. Anyone may create a task or attach a file, subject to daemon validation.

Assignment queues a task-correlated **brief** to the assigned agent, with only brief-kind files attached. Submission queues a **report** to the creator with the submission's files. These use the normal guarded, confirmed-message runtime; `queued mN` is **not delivery confirmation**. Read and acknowledge the resulting inbox message normally. An unchanged repeated operation is shown as `unchanged` and does not enqueue another message.

A task mutation and its automatic message enqueue are separate durable operations. If the task committed but enqueue was not confirmed, the successful response includes `notification: unconfirmed` with the daemon's diagnostic; text displays it prominently and JSON retains it. Inspect the task and inbox before resending. A daemon death between those writes can leave the task without its message, and replay does not re-emit old notifications. The CLI does not repair that gap by retrying. A notification status introduced by a newer daemon also stays a successful committed task: text shows the escaped status and explains that this build does not understand it. JSON passes the original result through without requiring the text renderer to understand its shape or notification vocabulary.

Mutations allow 35 seconds including startup/transport; list/get allow 15. A mutation timeout, lost connection or unreadable text answer does not prove rollback: accepted work may finish. Inspect `atm task list`, `atm task get <id>` and the inbox before considering another mutation; do not blindly resubmit. The shared transport cannot distinguish an early connection failure from one after the request was admitted, so mutation connection errors conservatively include this advice. Read timeouts are reported as read failures, not uncertain commits. Service/refusal/output errors exit 1 (also with `--format json`, with terminal-safe diagnostics on stderr); malformed command syntax exits 2 before contacting the daemon. A successful committed task with an unconfirmed or newer notification status still exits 0 and includes its notification outcome.

## Task reminders

When a named agent's turn ends and a task assigned to it is still `assigned`, `working` or `changes_requested`, `atmd` waits and then sends the agent one note. The note names the task and the commands that say where it stands: `atm task submit <id>`, or `atm task set-state <id> waiting --reason '...'` (or `blocked`). If the agent's next turn ends the same way, the task's creator gets one note. When the creator is `system` or the agent itself, the note goes to the agent's manager, or to you if it has none.

- A task that is `waiting` or `blocked` is not reminded: the agent has said why it stopped.
- It happens once per state. A change of state allows one more reminder.
- Nothing is sent before the agent has been idle for the delay, while its session shows background work, or while a message to it is still on its way.
- Each note is an ordinary message. It starts a model turn nobody typed, and the delivery guards hold it like any other message: a running turn, a draft, your focused pane.
- The event log records `task.reminded` and `task.flagged`.

```toml
[reminder]
enabled = true    # false turns reminders off
delay_secs = 60   # idle time after the turn's end before the note
```

`atmd` reads `[reminder]` when it starts. An unknown key or `delay_secs = 0` turns reminders off and logs the reason. They are also off while `[delivery]` is invalid: nothing could type the note.

Limits:

- A pending wait is kept in memory, so a daemon restart drops it. The marks are in the event log, so a restart never sends a second reminder.
- A turn whose end the harness does not report is not reminded: Esc during a reply in Claude Code or Codex, or a denied dialog.
- A session that reads idle while the harness still answers (a blocked `Stop` hook, an auto-compact) for longer than the delay gets a reminder it may no longer need, typed after the turn really ends.

## Intent and task measures

The owner writes and approves `<project>/.atm/intent.md` in the main checkout; agents do not create or change it automatically. Its **How we will know** section lists the measures against which tasks may be accepted. This is a syntax example, not a set of approved measures:

```markdown
# Project intent
## How we will know
- mechanical: `cargo test --workspace`
- reviewed: `.atm/measures/structure.md`
- owner
```

The pure parser supports ATX (`##`) and Setext headings, unordered/numbered list items, and optional matching inline backticks around command/rubric values. The heading is case-insensitive; the three measure keywords are lowercase. Nested headings stay within the section until a peer/ancestor heading ends it. Fenced/indented code examples and HTML-commented lines are ignored (including a whole declaration line with an inline comment). Values must be nonempty, control-free single lines; wrapped or lazy continuations immediately after a measure are refused, not truncated. `<!--` starts an ignored comment even inside inline backticks. This is a documented Markdown subset, not a general Markdown renderer. Duplicate sections, empty sections, malformed measure entries and unsafe rubric paths are refused; duplicate identical entries are harmless. Reviewed rubric paths must be canonical repository-relative files below `.atm/measures/`, without empty, `.` or `..` components or backslashes.

`task.create` reads that project's intent **on every call**, before committing a task. The typed measure must match a listed entry exactly: a mechanical command, a reviewed rubric path, or `{"kind":"owner"}`. Outside whitespace/backtick delimiters are removed; internal command bytes are not normalized. An owner caller is subject to the same measure list. Commands/rubrics are not executed or evaluated by this check; the existing task verdict rules still decide who may accept work. A refused create adds no task or event, and changes to the current intent do not invalidate replay of existing tasks.

A missing intent file preserves legacy unchecked creation. A **present** empty/malformed file or missing section does not disable validation. Intent reads are bounded to 256 KiB and 5 seconds; nonregular/symlink policy paths, invalid UTF-8, oversize files and read failures are explicit errors. No intent directory/file is created while checking policy.

The additive optional `project_id` parameter selects a **registered** project for owner creates. Named callers use their own project and cannot override it with another id. For an owner with no selector, a sole registered project is used; multiple projects require `project_id`, and no registered projects preserve legacy unchecked creation. An unknown selected id is `not_found`. This selector is a method parameter, not a task/event field; the daemon never guesses the project from its own working directory.

For example, after the normal 1.1 handshake, an unbound owner can create a task in a registered project with this one-line request (the command must be listed in that project's intent if one exists):

```json
{"protocol_version":{"major":1,"minor":1},"type":"request","id":"task-1","method":"task.create","params":{"project_id":"example","title":"Check the change","measure":{"kind":"mechanical","command":"cargo test --workspace"}}}
```

Intent, standing rules, rubric files and their admission/append judges are on the [protected-file report list](scripts/atm-promotion.md#protected-file-report). Keep project rubric files under `.atm/measures/` and mechanical judge scripts under the already-protected `scripts/` directory. Reporting detects committed endpoint changes; it does not prevent same-user writes or discover arbitrary command dependencies. Setup does not install a project intent or rubric on the owner's behalf.

## Decisions

Named agents raise durable questions from their **bound agent pane**:

```sh
atm decisions request --kind plan --title 'Approve this plan' \
  --option yes --option no --recommended yes \
  --may-be-undone true --changes-intent true \
  --context 'Choose the smaller interface' --attach /path/to/plan.md
atm decisions withdraw d41 --reason 'Superseded'
```

`--kind` and `--title` are required; repeat `--option` for 2–32 distinct options. Both `--may-be-undone true|false` and `--changes-intent true|false` are required valued flags, with **no assumed safety facts**. The daemon derives the class and applies the existing [away policy](#away). Context defaults to empty, attachments to an empty list, and recommendation/deadline to null. Repeat `--attach` for literal paths: nothing is read, uploaded or canonicalized; use absolute paths across worktrees. Optional `--deadline` takes a future RFC3339 timestamp (offsets normalize to UTC); the daemon checks its time, and storing it does not start an expiry timer. No requester/by/from override is supported.

Request text output is `d41 requested`, a receipt of creation, **not** approval, a Pending-state guarantee, or notification delivery. Away handling may already have taken it provisionally. Withdrawal is allowed only for a **Pending** decision, by its requester or the owner; provisional/final decisions cannot be withdrawn. Repeating an authorized withdrawal reports `(unchanged)` and preserves the original withdrawal facts, even if a different reason is supplied.

List visible requests, or answer as the owner from a genuinely **unbound** shell:

```sh
atm decisions                         # pending + taken provisionally
atm decisions --status resolved        # repeat --status to combine states
atm decisions --all --format json
atm decisions --after d41 --limit 20
atm decide d41 yes 'Use the smaller interface'
atm decide d41 yes 'Always use this shape' --rule --format json
```

IDs are `d` plus the request event's sequence (`d41`, not `d041`). Options, context, recommendations, notes and reasons are exact strings: quote multiword values; there is no trimming or case folding (`--option=-value` accepts a leading hyphen). `--format json` preserves the complete daemon result, including additive fields/nulls; text escapes terminal controls. Put mutation format after the subcommand: `atm decisions request … --format json` or `atm decisions withdraw d41 --format json`. Parent/list flags cannot be combined with mutations; `--all` conflicts with `--status` on lists.

A timeout, disconnect, invalid result or output-write failure is **not rollback**. Inspect `atm decisions --all` and your inbox before retrying: every accepted request creates a new ID, and the CLI never retries automatically. Request/withdraw use the existing API below; no extra manual question message is needed.

An agent pane sees its own requests; an unbound owner caller sees all. Pending and provisional decisions remain on the default list, with separate `pending_count` and `provisional_count` totals. Counts cover the caller's scope, not just the page or status filter. `atm status` independently asks for the global **Pending-only** count, even from an agent pane: nonzero adds `N decisions`, zero adds nothing, and missing/unavailable/bad aggregate data adds `? decisions` plus a diagnostic without hiding session or unread counts. A provisional decision is not counted as pending; it remains in `atm decisions`.

Only the owner resolves. A pane bound to an agent is refused; an unbound pane (or absent pane) counts as owner, just as for messages. This is **attribution, not authentication**: clients supply their pane, and processes running as the owner can claim an unbound one. Resolved/overruled log bodies include the optional caller pane; the fold and repeat rule ignore that audit metadata. Old bodies without it still replay unchanged.

The exact same answer (option, note and rule mark) again is an accepted decision no-op. A changed answer after a final resolution is refused, including adding a note or `--rule` afterwards. A different answer to a provisional choice is recorded as `overruled`. A stored deadline does not run an expiry timer. What happens to a decision while you are away is described under [Away](#away).

### Standing rules

An accepted owner `atm decide … --rule` also appends the final question, answer and optional note to the **requester's registered main project** at `.atm/rules.md`, creating the policy directory/file if necessary but never recreating a missing project checkout. A projectless system request cannot become a standing rule; that refusal happens before resolution. Ordinary answers, agent callers and rejected answers do not touch this file. Existing historical rule marks are not backfilled on startup: the owner can repeat the identical marked answer to publish one.

Appends preserve existing UTF-8 text and mode bits. A whole-line decision-id marker deduplicates identical/concurrent retries, including after restart; user question/note lines are quoted so they cannot manufacture another marker. Daemon writes are serialized through one retained filesystem slot, limited to 5 seconds and 1 MiB including the new entry. Nonregular/symlink policy paths and invalid UTF-8 are refused. Publication writes and syncs a same-directory temporary file, renames it atomically and syncs the policy directory (and its project parent when preparing the directory), so a partial marker is never presented as a complete rule. Coordinate manual edits separately: they are not transactionally merged with an in-flight daemon write.

The event log and project file are **not one transaction**. Success confirms both the resolution and rule publication. If publication fails after the decision commits, the error says the resolution committed and the append was not confirmed: fix the file problem and retry the **identical option, note and `--rule`**. The decision stays unchanged while a missing rule is repaired once. The same explicit retry repairs a daemon crash between commit and publication; there is no background rules-file rewrite. Admitted resolve/append work retains its capacity after RPC cancellation/timeouts, but an error/timeout is not a rollback—inspect the decision and file before retrying.

### Decision API

After the normal 1.1 connect handshake, a named agent can send this request envelope (one JSON line):

```json
{"protocol_version":{"major":1,"minor":1},"type":"request","id":"question-1","method":"decision.request","from_pane":"%7","params":{"kind":"plan","title":"Approve this plan","context":"Choose the smaller interface","files":["plan.md"],"options":["yes","no"],"recommended":"yes","may_be_undone":true,"changes_intent":true,"deadline":null}}
```

`requester` and `by` are never accepted params; the daemon derives them from the caller pane. Owner/unbound callers cannot request; internal system requests are separate. All params reject unknown fields; IDs and timestamps are validated. New kinds must be `action`, `hire`, `budget`, `policy`, `plan` or `dialog`; later kinds replay as `unknown` but cannot be newly requested through this daemon.

| Method | Params | Result |
|---|---|---|
| `decision.request` | Required `kind`, `title`, `options`, `may_be_undone`, `changes_intent`; optional `context=""`, `files=[]`, `recommended=null`, `deadline=null` | `{"decision_id":"d41"}` |
| `decision.list` | Optional `statuses=null` (pending + provisional), `after=null`, `limit=100` (1–1000), `count_only=false` | `decisions`, `pending_count`, `provisional_count`, `next_after` |
| `decision.get` | `id` | `{"decision":<entry>}` |
| `decision.resolve` | `id`, `option`; optional `note=null`, `standing_rule=false` | `{"decision":<entry>,"changed":true}` |
| `decision.withdraw` | `id`; optional `reason=null`; requester or owner only | Same mutation result |

An entry reuses the decision's `id`, `request`, timestamps, `status`, provisional/resolution/withdrawal facts, and derived `class`. Status is authoritative; historical logs may retain an earlier fact under another status. Optional facts are null. `count_only=true` returns empty rows/null cursor and global owner counts to any caller; it excludes `after` and explicit `statuses`. Pages/get are bounded to 512 KiB without truncating facts; an oversized first row is refused. Continue with `next_after` while preserving your filters. There is no project filter for a system request with no structured project.

Bounds are UTF-8 bytes: title 1–512; context ≤16384; 2–32 distinct nonempty options, each ≤1024; ≤32 nonempty file paths, each ≤4096; note/reason ≤8192. Recommendation must match an option exactly and a request deadline must be in the future. Unknown IDs return `not_found`, invalid params `invalid_params`, policy/identity refusals `refused`, unavailable persistence `unavailable`, and timeouts `timeout`. Accepted work can finish after timeout: inspect the list/get/inbox before retrying, rather than creating another request.

### Answers and persistence

The org event log retains requests and answers across daemon restarts. Recording an answer does **not** mean a harness accepted it: one daemon bridge durably enqueues a `system` / `answer` message to the requester, then the existing FIFO/guard/confirmation runtime delivers its one-line inbox reference. Read the body with `atm inbox --id m…`, then explicitly acknowledge it. Focused panes and occupied editors stay protected; no separate decision paste path bypasses those guards. Internal `system` requesters have no inbox and consume the durable resolution event instead.

The final decision is the bridge's recoverable work item; an exact canonical answer `msg.sent` is its completion marker, regardless of delivery/read status. Restart fills a missing enqueue and does not duplicate an existing one. An organisation snapshot or enqueue timeout backs off and reconciles from fresh committed state; the serial actor and exact-content check prevent duplicate late enqueues. A stopped actor or other terminal bridge failure requires restart; reads remain available when org persistence is healthy. Delivery failures/holds retain the normal message recovery rules. The canonical answer text is therefore a persistence correlation contract, not presentation-only text to change without considering old logs.

New pending requests also run the owner's existing `[notify]` command, using a short `d41 from project/worker: atm decisions` reference, not private title/context/options. This shares the owner-message notifier described below; resolved, provisional, withdrawn or expired decisions suppress a delayed notice. External notices remain best effort, not the durable decision record.

## Away

```sh
atm away      # from a shell that is not an agent's pane
atm back
```

`atm away` tells the daemon you will not answer. Until `atm back`, every pending decision, whether asked before you left or after, is put through this table. While you are present nothing is ever taken for you.

| The request says | While you are away |
|---|---|
| It can be undone and does not change the intent (a detail) | The agent that asked decides: its own recommended option is recorded, `by` the agent |
| It can be undone and changes the intent | Taken provisionally on the recommended option, `by` `system` |
| It cannot be undone or reaches outside | Waits for you |
| Its kind is `action`, `hire` or `budget`, whatever else it says | Waits for you |
| It names no recommended option | Waits for you |

The two fields that give a decision its class are filled in by the agent that asks, so the kinds that reach outside wait whatever they claim. Both recorded outcomes are the existing `taken_provisionally` state: they stay in `atm decisions`, and `atm decide` with the same option confirms one (`resolved`) and with another overrules it (`overruled`). `atm back` takes nothing back and answers nothing; it prints how many decisions wait for your answer. A digest of what was decided is not built yet.

The requester is told through the confirmed-message runtime, like an answer: one `system` message when its decision is taken provisionally, is its own to decide, or waits because you are away. `decision.request` itself answers as before. Your `[notify]` command still runs for a new pending decision.

**The stacking cap.** A provisional decision may carry the task in progress to its end; nothing else may be built on it until you have answered it. A task built on a decision names it in `rests_on` when it is created. Such a task may be created and stays `open`, but `task.assign` refuses it (`refused`) while the cap is reached, and lifts by itself once you answer the decision; being back is not enough. The cap counts tasks, the task in progress being the first: with the default of 1 no further task starts, with N, N − 1 may. Only a provisional decision that changes the intent is capped. A task already assigned runs on. Known limits: a task that does not name its decision is not seen, and chains (a child of such a task, a second decision raised inside one) are not followed.

The cap is a setting per project, in `<project>/.atm/away.toml`:

```toml
stacking_cap = 1
```

No file means 1. The daemon reads the file of every registered project when you run `atm away`, records the caps with the switch, and prints them; a change takes effect at the next `atm away`. A value below 1, an unknown key or a file that is not TOML refuses `atm away` with the file's path, and you stay present. A decision's project is that of the agent that asked.

Only the owner sets the switch: as for `atm decide`, a pane bound to an agent is refused and an unbound pane counts as you. This is attribution, not authentication. The switch is in the org event log (`owner.away`, `owner.back`), so it survives a daemon restart; after one, the daemon handles whatever was left pending. Repeating either command changes nothing and says so. `--format json` prints the daemon's result. While you are away `atm status` ends with `away`, so a forgotten `atm back` shows in your status line; the mark comes with the decision count (`decision.list` adds `"away": true` to its result, and only then), so it is absent when that count cannot be read.

| Method | Params | Result |
|---|---|---|
| `owner.away` | none | `changed`, `away_since`, `stacking_caps` (every registered project), `pending_count` (as the call found it) |
| `owner.back` | none | `changed`, `away_since` (null if you were not away), `provisional_count`, `pending_count` |

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

For Owner inbox messages and pending decisions, one consumer subscribes to committed events before service admission, skips startup history and deduplicates replay overlap within the process. It notifies new Owner messages and decision requests; OwnerFocused notices remain **inbox-only** (no `[notify]`). A message notice contains bounded metadata and an inbox reference with `--for owner`, not the body; a decision notice points to `atm decisions`. `delay_secs` delays launch, and committed unread/pending state is checked again just before launch: reading a message or closing/taking a decision while queued suppresses it, as does an unavailable state read. The shared queue has one running/delayed command and at most 32 pending commands; overflow is logged without changing durable state. Commands use the same no-shell runner and 30-second timeout as attention notifications, which retain their independent per-session behavior. Notification failure never generates another message or retries the command. This external effect is **best effort**, not durable/exactly-once: shutdown cancels pending work and a crash can lose a notification. A watch error stops Owner notifications until restart and logs one warning; consult the inbox/status for durable state.

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

`atm setup` installs four skills for Claude Code, pi and Codex:

- **Named agents:** `atm-lead` and `atm-member` use `atm me`, durable tasks,
  confirmed messages, decisions and the away rules. They are **drafts,
  unevaluated for model compliance**, not an unattended-operation guarantee.
- **Legacy unnamed panes:** `atm-coordinate` and `atm-implement` keep the
  existing `list`/`peek`/`send`/`reply` loop. Both remain available; do not mix
  that loop with the named-agent protocol.

Invoke a named skill at the harness input as `/atm-lead` or `/atm-member`
(Claude Code), `/skill:atm-lead` or `/skill:atm-member` (pi), or `$atm-lead` or
`$atm-member` (Codex). Claude skill calls must stay within the recorded
800-character/3-line limit; use one short line and keep briefs in files.

After a start/clear, `atm me` finds the role and memory but does **not** load a
skill. An approved role instruction body can direct reading its installed
`SKILL.md`; `skills = [...]` alone does not. Keep current priming until that
bootstrap is verified on the harness/model. Installing skills does not adopt
agents or enable automatic handover. Decision creation is API-only (no request
CLI), named leads cannot inspect every agent's decisions/inbox, and the A2
digest/A3 drift scheduler are not built. The skills expose these limits rather
than silently bypassing them.

Source, per-rule reasons/model/harness verification records, installation and
usage: [`crates/atm/assets/atm-skills`](crates/atm/assets/atm-skills/README.md).
`atm uninstall` removes all four ATM skill folders, leaving unrelated skills.

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
