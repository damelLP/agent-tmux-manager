# Agent Tmux Manager (ATM)

[![Build Status](https://github.com/damelLP/agent-tmux-manager/actions/workflows/release.yml/badge.svg)](https://github.com/damelLP/agent-tmux-manager/actions)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A live dashboard and a daemon-backed control plane for coding agents in tmux.

**0.5.2 polishes the Work view, recognises more Claude Code screens, and prompts new sessions to resume after a saved handover.** ATM also includes owner decision answers and spoken-answer relays, message history, return digests, and evaluation reports. It is not a complete unattended workflow: see [known limits and not yet](#known-limits-and-not-yet).

## What it does

ATM gives you a live dashboard and CLI for coding agents running in tmux, including Claude Code, pi, and Codex CLI. See context usage, cost, model, and activity at a glance — and control agents without switching panes.

- **Dashboard** — Work-first reporting threads and Sessions tabs, context bars, cost tracking, and live terminal capture
- **Agent control** — spawn, kill, interrupt, send text, and reply to prompts from the CLI
- **Named agents** — project-local roles with stable `project/name` identities, start/adopt/clear/respawn commands, and save-before-clear handover
- **Durable coordination** — confirmed message references and inboxes, scoped history, project-attributed tasks with reviews and observed usage
- **Owner choices** — TUI decision answers, provisional spoken-answer relay, standing rules, away/back return digests and evaluation reports
- **Owner chat** — separate, window-first direct messages using the shared Owner inbox and existing delivery service
- **Workspaces** — create tmux sessions with built-in ATM sidebars, or inject sidebars into existing sessions
- **Layouts** — preset multi-agent arrangements (solo, pair, squad, grid) with one command
- **Tmux native** — status bar integration, popup picker, vim-style keybindings

Named coordination is not a security sandbox or a guarantee of unattended agent behavior. A delivery receipt means the harness accepted a message reference, not that the model read it or completed the task. The named-agent skills are drafts; keep human review in the loop.

- [Install and setup](#install-and-setup)
- [Named-agent/project quick start](#named-agentproject-quick-start)
- [Upgrade](#upgrade)
- [Dashboard and daemon](#dashboard-and-daemon), including [Work view](#work-view)
- [Messages and inbox](#confirmed-messages-and-inbox), [history/search](#message-history-and-search) and [owner chat](#terminal-chat)
- [Decisions and relay](#spoken-answer-relay), [away/back](#away) and [evaluation reports](#evaluation-report)
- [CLI reference](#cli-at-a-glance) and [organisation overview](#organisation-overview-atm-org)
- [Known limits and not yet](#known-limits-and-not-yet)
- [Source builds and rollout](#building-from-source-and-rolling-out)
- [Changelog](#changelog)

## Install and setup

### Linux with Cargo (recommended)

Building from crates.io requires **Rust 1.88 or later**. You also need tmux and an installed supported harness; the shell hooks use `jq` and either `socat` or `nc`.

Initialize/configure your chosen harness in this HOME first, including any provider/login setup it requires. Having its executable on PATH alone is not enough for ATM setup to detect it.

Install the **`atm` crate** from crates.io. It provides both programs, **`atm` and `atmd`**; `atmd`, `atm-tui`, `atm-comms` and `atm-chat` are library-only crate names, not separate program installation targets. See [how the shared libraries fit](#how-it-works).

```bash
cargo install --locked atm
atm setup
```

Cargo normally installs the pair in `~/.cargo/bin`. Make sure that directory is on `PATH`, then complete [setup verification](#verify-harness-setup) before the quick start.

### Optional prebuilt archives

The release workflow also builds **Linux x86_64 and aarch64, glibc** archives. It does not publish macOS or Windows binaries. The Bash installer downloads the latest published release, installs `atm`, `atmd` and `atm-hook` in `~/.local/bin` by default (`ATM_INSTALL_DIR` overrides it), then runs setup:

```bash
curl -fsSL https://raw.githubusercontent.com/damelLP/agent-tmux-manager/main/scripts/install.sh | bash
```

Use one installation of the **pair**, `atm` and `atmd`, with its directory first on `PATH`. Check `command -v atm` and `command -v atmd` if you have used both routes. Installer completion is not a substitute for the setup verification below.

### Verify harness setup

Run `atm setup` and verify that its **Detected coding agents** output marks your chosen harness as present before proceeding. If it reports **No supported agent installations found**, initialize that harness in this HOME and rerun setup. Detection uses the harness home directories, not executable presence on PATH: setup's zero exit status or the installer's **Installation complete!** message alone does not prove an integration was installed.

For detected harnesses, setup installs these integrations:

| Harness | What setup installs | After setup |
|---|---|---|
| Claude Code | `~/.local/bin/atm-hook`, hooks and an ATM statusLine only if yours is absent | Keep your own statusLine; forward its stdin to `atm-hook` for context/model/cost ([details](#how-it-works)) |
| pi | Local `pi-atm` package in `~/.pi/agent/packages/pi-atm/`, registered in `settings.json` | Restart pi to load changed extension code |
| Codex CLI | `~/.local/bin/atm-codex-hook` and `~/.codex/hooks.json` | Approve the installed hooks with `/hooks` inside Codex |

For pi, before starting or restarting it, verify that `~/.pi/agent/packages/pi-atm/package.json` and `~/.pi/agent/packages/pi-atm/extensions/pi-atm.ts` exist, and that the `packages` array in `~/.pi/agent/settings.json` contains `"packages/pi-atm"`. If any are missing, rerun setup and check its output.

It also refreshes four ATM skill folders for each detected harness, writes tmux bindings, and creates ATM's config only if absent. With no supported harness directory it reports that and makes no setup changes. Setup does not start or adopt named agents, create project intent, or enable automatic handover. It replaces ATM's installed skill folders whole: do not keep local edits there. It writes the bindings file but does not source it into a running tmux server.

Codex launches through ATM require codex-cli **0.158.0 or later** for `--no-daemon`; ATM adds the required argument.

The pi integration forwards `agent_settled` (introduced in **pi 0.80.4**) instead of the earlier `agent_end` for final idle. Use **pi 1.5.2 or later** (1.5.2 tested), as required by [pi-atm](extensions/pi-atm/README.md#forwarded-events). Without `agent_settled`, pi rows can remain Working after every turn. Follow [Upgrade](#upgrade) before refreshing the forwarder.

## Named-agent/project quick start

Run these commands from an ordinary, **unbound owner shell inside tmux**, in the project's main checkout. First complete [harness initialization and setup verification](#verify-harness-setup) in this HOME, including the package/registration check if using pi. Do not proceed just because setup exited successfully. Starting an agent starts that harness and can initiate a model turn; this is not a dry run.

1. Create `.atm/roles/` and a **new** `.atm/roles/worker.md` (choose another name if it already exists):

   ```markdown
   +++
   harness = "pi"
   cwd = "."
   +++
   # Worker
   At each new session run the shell command `atm me`, read your role and
   existing memory, then read the indicated inbox messages. Follow the brief;
   ask the owner about missing requirements. Acknowledge delivered messages
   with `atm msg ack <id>` after reading them.
   ```

   The `+++` section is TOML; the rest is the role's instruction body. `harness` can instead be `claude` or `codex`. An omitted manager means reports go to the owner. `skills = [...]` is metadata, not automatic skill loading.

2. Start and inspect it; leave the new pane unfocused until the launch completes:

   ```bash
   atm agent up worker --project-root "$PWD"
   atm agent list
   ```

   The first use registers the project and writes `.atm/project.toml` with its id. A named agent is addressed as `project/name`; the bare `worker` resolves in this project's context. `up` prints the agent id, pane and state (or use `--format json`). A `stuck` start needs inspection, not an assumption that the harness is ready. Folder trust is accepted only for the role's explicit `trust_dirs`; other startup questions can need your answer. See [Starting a named agent](#starting-a-named-agent) for launch limits.

3. Put the actual task in `brief.md`, then send a reference from the owner shell:

   ```bash
   atm msg send --to worker --kind brief --body 'Read the attached brief and report the result.' --attach "$PWD/brief.md"
   atm inbox --for worker
   ```

   Attachment paths are literal references, not uploaded file contents. `queued` is not confirmation: inspect the inbox's delivery status. The agent receives `[mN] Read: atm inbox --id mN`, retrieves the body, and acknowledges only once Delivered. Focused panes, drafts, dialogs and running turns can hold delivery. Use `atm inbox --for owner` for the agent's report.

4. Save context before replacing it, or stop it when finished:

   ```bash
   atm agent handover worker     # asks it to save; clears only after the file changes and the turn ends
   atm agent down worker --kill  # stops the named agent and closes its pane
   ```

After a successful start/clear, ATM queues `[mN] New session: run the shell command atm me`. From the bound pane, `atm me` finds the current role, `.atm/memory/worker.md`, unread work and reporting command. It does not itself load a skill or acknowledge messages. Owner-only commands such as `atm decide`, `atm away` and `atm back` must come from the unbound owner shell, not an agent pane.

## Upgrade

1. Quit **every** ATM TUI/sidebar and stop other callers that could autostart the daemon. Save important agent context and avoid upgrading during a start, clear or message delivery.
2. Run `atmd stop` with the **existing installation and environment**. Wait for confirmed exit; a timeout does not mean it is safe to swap binaries. An open TUI or another CLI caller can start it again.
3. Preserve the existing state directory and configuration. Replace `atm` and `atmd` together using one install route. Do not erase the organisation log to make an older binary start.
4. Run the newly installed `atm setup` to refresh embedded hooks, pi extension and skills, and complete [setup verification](#verify-harness-setup) before proceeding. Check the pi package/registration before restarting pi, and approve Codex hooks if prompted. Existing ATM config and custom Claude statusLine are preserved, but ATM-owned skill folders are replaced. Setup does not restart the daemon; the refreshed integrations need the matching daemon restarted too.
5. Start `atmd start -d` from the intended environment, then start new pi sessions to load the refreshed extension. Reopen sidebars, and check `atmd status`, `atm list`, `atm status`, `atm agent list` and the inbox. For source-checkout deployments use the [rollout tools](#source-checkout-rollout) below instead of manually copying over a running pair.

Protocol **1.1** retains major-1 hook compatibility, including the existing 1.0 hook frames. That is not a promise that an old daemon has the new commands, or that an older binary can replay newer persistent state. A restart cannot recover historical data that an older release only kept in memory; some session metadata returns only on the harness's next event. Back up important persistent state while the daemon is stopped before trying a downgrade.

## Known limits and not yet

- **Pi clear:** `atm agent clear` works with pi's **Update Available** notice on built-in themes. With a custom theme whose notice ATM cannot recognise (neither faint labels nor a bold title), clear still refuses safely; dismiss the notice or type `/new` by hand. A refused clear is not a fresh session.
- **Pi turn completion:** pi reports final idle through `agent_settled`, but idle input consumed by a later extension handler can still leave a row Working without an agent run. Guarded reconnect idle snapshots do not override that pending-input veto: it lasts until a genuine run/session boundary. The separate input idle correction remains deferred because it double-counted turns and could loop automatic handover. Inspect the pane rather than treating a status label as proof that a turn has settled.
- **Named skills and automation:** `atm-lead`/`atm-member` remain drafts, unevaluated for model compliance. Role metadata and `atm me` do not load a skill, and setup does not enable automatic handover. Keep human review and the documented guards.
- **Relay visibility:** a successful relay or same-option repeat returns the decision entry (title, context, attachment paths and options) even when `decision.get` would refuse that seat outside the requester's reporting line. An `AlreadyTaken` refusal reveals the recorded option, enabling that repeat. When the relay already has reporting-line visibility, this adds none; otherwise designation can widen what that seat sees. See [spoken-answer relay](#spoken-answer-relay).
- **Implemented reports / not yet:** the [return digest](#away) from `atm back` and [evaluation report](#evaluation-report) from `atm report` are available. A drift scheduler is not shipped, and the remaining unattended/end-to-end coordination acceptance work is not verified. These commands do not complete a broader review-automation workflow or guarantee unattended operation.

## Dashboard and daemon

```bash
atm                    # launch TUI (starts daemon automatically)
```

Sessions appear as you use supported coding-agent harnesses. The full dashboard and compact sidebar start on **Work**, followed by **Sessions**. Press `gt` / `gT` for the next / previous tab (wrapping), `1gt` / `2gt` to select Work / Sessions, or `w` as a toggle alias. Press `Enter` to jump to a session, `q` to quit.

An open TUI restarts the daemon within a few seconds if it goes away, including after `atmd stop`. Quit every TUI before you stop the daemon for good, or before you restart it by hand: a TUI may start it first, with the TUI's environment. The daemon log defaults to `~/.local/state/atm/atm.log` (or `$XDG_STATE_HOME/atm/atm.log` when `XDG_STATE_HOME` is absolute); each background start keeps the previous run's log as `atm.log.1`. Set `ATM_STATE_DIR` to a nonempty directory path to keep the daemon's PID file (`atmd.pid`), logs (`atm.log`, `atm.log.1`) and event log (`log/`), and the TUI's `tui.log`, there instead. Use a directory that survives a reboot: atmd replays the event log at start.

`atmd stop` waits for the daemon's real process exit, not just removal of its socket or PID file. After 5 seconds it prints one progress line and continues waiting, up to 75 seconds from signaling. A nominal shutdown can take 66 seconds: connections (2); then reserved work (agent lifecycle flows, observations, message deliveries) is stopped at once and only its cleanups go on, while messages, agents and control stop in that order, by 20, 22 and 62 seconds from that moment, the last covering admitted control input under its original deadlines (30) and audits (30); then concurrent summary/raw flushes (2). A stage that passes its bound is logged and abandoned, not waited for. A clear that the stop interrupts leaves its agent stuck, with what the clear last waited for. Blocking I/O, including work that Tokio waits for at runtime shutdown, can take longer without a finite bound. On timeout, stop exits non-zero and names the PID that may still be shutting down; it neither force-kills the daemon nor removes its state. Successful stop confirms exit, not that every pending write was persisted.

Known limit (unverified in a seccomp-filtered runtime): if a filter rejects Linux `pidfd_open` with `EPERM`, `atmd stop` fails before signaling the daemon. Only `ENOSYS` enables the old-kernel numeric-PID fallback; a seccomp denial does not.

### Work view

Work defaults to **reporting threads**: each lead with registered reports heads a group containing its own session and reporting seats; nested leads get their own groups, including across repositories. The headline is the lead's first known open-task title, falling back to its name. Named rows likewise put the task title first, with the agent name and repository/worktree as dim detail; `+N`, decision and attention marks retain their counts. **Setting a custom short work title is not implemented yet**, pending the separate title-setting contract; these are task-title/name fallbacks, not new work records.

Inside Work, press **`gw`** to switch between reporting threads and physical tmux windows. Unmatched, ambiguous or broken reporting lines retain window groups; sessions without a known window appear under **`(no window)`**. Groups summarize open-task and decision counts and attention. These recorded projections are not proof of execution, and reporting groups do not add authority.

Use **j/k** to navigate, **Enter** to fold a group or jump to a session, **`/`** to filter, **zc/zo/za** to close/open/toggle a fold and **zM/zR** to collapse/expand all. **n/N** visit attention and waiting-decision rows, opening their ancestors within the current filter. The filter is shared between views; each view keeps its own folds. **`r`** refreshes.

Work reads `session.windows` and `org.tree` through the daemon. Failed reads keep the last good data and show **unavailable**, with a **stale** age once its freshness threshold passes. On the first connection to a daemon without `session.windows`, an untouched startup choice falls back to Sessions once; selecting Work still shows its needs-newer-`atmd` message. Reconnects do not change your chosen tab. Without `org.tree`, Work shows **no org data** and uses window grouping, not invented task counts.

## CLI at a glance

Data and one-shot commands exit 141 quietly when stdout's reader closes (including with backtraces enabled; `pipefail` exposes this status in a pipeline); accepted work is not rolled back, and `spawn` still finishes before reporting the failure. `setup`, `uninstall` and `layout` do not yet share this closed-pipe policy.

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
atm org                            # organisation, reporting lines and what needs the owner
atm report --since 7d              # outcomes, time, owner step-ins, cost and change trends
atm decisions                      # pending/provisional decisions for you
atm decide d41 yes 'Approved'      # owner answer; --rule appends a standing rule
atm away                           # you will not answer: agents go on under the away rules
atm back                           # you are back: nothing more is decided for you
atm status                         # one-line summary for tmux status bar
atm usage                          # latest observed subscription usage
atm usage --format json            # the same readings as a JSON array

atm workspace create               # new session with ATM sidebar + agent + shell
atm workspace attach               # inject sidebar into current session
atm layout pair                    # two agents + ATM sidebar
```

Workspace creation and attach setup, including generated resize/new-window hooks, intentionally control tmux client-side on the selected server (`--isolate`, otherwise `ATM_TMUX_SOCKET` or the inherited/default server). This is an exception to daemon-owned control beside terminal attachment and view-only navigation; daemon-backed operations still refuse a different tmux server.

`atm layout` likewise applies a layout client-side on the selected tmux server and needs no daemon; with `--session` it can start a new server.

Known reply limit: `-y`/`-n` can wait behind reserved work and do not re-check the screen when released, so their keys go to whatever is then visible.

`atm role` sets or clears pane-local `@atm-role` directly through tmux: an implicit `$TMUX_PANE` or explicit `%N` target needs no daemon, while a session-id target uses atmd only to resolve the pane. This intentional exception to daemon-owned control preserves the tag across a clear or harness restart; without `$TMUX_PANE`, pass `-t`. `atm list -f json` adds `role: "coordinator"` when a session's pane is tagged and its parent does not share that pane, or `role: null` otherwise (also on failed role lookups, including missing daemon capability or a server mismatch). No parent, or a parent missing from the full session list, passes: a separate-process teammate on its own tagged pane can coordinate, but an in-process subagent sharing its parent's pane stays null, even when filters hide the parent. Table and ids formats are unchanged. `/atm-coordinate` tags its pane during setup. The TUI shows a bold cyan `C` on coordinator rows and puts them first within each agent group, keeping newest-first order within each band. Nested teammates stay in place. The TUI and JSON list read tags through atmd on the caller's verified tmux server; the TUI also writes tags through atmd. Press `c` to toggle the selected row's pane; group rows, rows without a pane and children sharing their parent's pane are unchanged. External tag changes appear on the next three-second poll; `c` updates the display immediately. While a write is pending, another `c` is ignored. A failed write leaves the optimistic display until a successful poll reconciles it; a failed poll keeps the last tags.

### Starting a named agent

```bash
atm agent up worker                          # start the agent of role "worker" in this project
atm agent up worker --project-root ../other  # ... in another project
atm agent clear worker                      # clear the agent's current context
atm agent handover worker                   # ask it to save its state, then clear it
atm agent respawn worker                    # replace its pane and start it again
atm agent down worker --kill                # stop the agent and kill its pane
```

`atm agent down <agent>` releases the seat and leaves any existing pane and its program running; `atm agent down <agent> --kill` closes them instead.

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

`atm agent handover <agent>` does in one command what a clear by hand needs three steps for. `atmd` sends the agent one message that asks it to write what its next session needs to `.atm/memory/<name>.md` in the project's main checkout. It waits until that turn has ended and the file has changed, and then clears the agent as `atm agent clear` does. Instead of the plain start line, the replacement is prompted: `Run atm me; read its memory file; resume open work or report and stop.`

```bash
atm agent handover worker                      # waits up to 10 minutes for the save, then clears
atm agent handover worker --timeout-ms 120000  # 30000 to 1800000
atm agent handover worker --format json        # {"agent_id":…,"message_id":"m12","state":"cleared","session_id":…}
```

- **Nothing is cleared unless the agent saved.** `atmd` notes the memory file's size and modification time when the command starts, just before it queues the request. Saved means that, once the save turn has ended, the file is there and either differs from that or is new. A change made before the request reaches the agent counts too, for example by an agent that writes its file and asks for its own handover in the same turn. The content is not read. If the save turn ends and the file is as it was, or it has been removed, the command fails with `<agent> ended its save turn and <file> did not change; nothing was cleared`.
- **The request is an ordinary message** from `system`, typed as `[mN] Read: atm inbox --id mN` under the usual delivery rules: an idle agent, an empty editor, no dialog, and not the pane you are focused on. If it is not delivered within the timeout the command fails and says so. The request stays queued, the next `atm agent handover` of that agent reuses it, and `atm msg give-up mN` cancels it.
- **If the save turn does not end in time** the command fails with `<agent>'s save turn did not end in <n> s`, and adds `it waits at a dialog in %N` when the agent is waiting for an answer, for example a permission to write the file.
- **A saved handover waits for a safe editor.** If the first clear preflight sends nothing because the pane is busy, focused, at a dialog or has a draft, `atmd` can check again for up to 20 seconds on the same pane and session. `atm agent list` shows why it is waiting. It never asks for a second save, deletes a draft or repeats a clear that may have sent input. A changed session or lost pane stops it. On failure the command identifies the save and the refusal; inspect `atm agent list` before trying again.
- **Cleared is not the same as resumed.** The command still answers `cleared` when the clear itself is confirmed. For the outcome note, `atmd` waits up to another 30 seconds for both the new session's binding and a receipt for its exact start line. A success names the save message, old and new session IDs and delivered resume message. If the start line is disabled, held, or unconfirmed, the note says the clear succeeded but resume was not confirmed. It does not resend a prompt or send a second note if the receipt comes late. A receipt proves prompt acceptance, not that the agent read its memory.
- **Who hears the outcome.** After a save, the current role's manager gets one outcome note, falling back to the recorded manager at `agent up`, then to the owner. An explicit `manager = "owner"` wins. Before a save, a waiting command gets its error as an answer; an unclaimed failure goes in a note. From its own pane the command answers `started` at once so the agent can finish its turn, and the daemon carries on. From an agent's shell, whose tools may stop a long command, run a handover in the background.
- **Only a `ready` agent with a known session** can be handed over, and one handover per agent runs at a time. Anyone may ask, as for messages.
- **A restart of `atmd` during a handover** loses its in-flight work and its outcome note. The saved request or a clear already started may still have taken effect. Inspect the agent and its inbox before rerunning the command.

**A note at the context line.** When a named agent's turn ends and its context is at 60% or more, `atmd` sends one note to the agent's manager (to you for an agent whose role names none, and for an adopted agent): `agent <id> is at <n>% of its context (line 60%). Hand it over: atm agent handover <id>`. It is sent once per session and nothing is cleared. After a restart of `atmd` a session that was already noted can be noted once more. The line is 60% for every harness and model: a chosen number, not a measured one, until measurements give better ones.

**Handover without a command.** A role file can let `atmd` hand its seat over by itself. It is off unless the role says so:

```toml
auto_handover = ["context"]   # or ["context", "idle"]; leave it out for off
context_line  = 60            # optional: this seat's line in percent, 1 to 100 (default 60)
cache_minutes = 60            # optional, required for "idle": how long you assume the prompt cache lives
idle_minutes  = 48            # optional: default four fifths of cache_minutes, and always below it
```

- **`"context"`:** when a turn of the seat ends at or above its line, `atmd` runs the same save, clear and resume check. The manager gets one outcome note in place of the note at the line, for example `automatic handover of p/worker (context 63%, line 60%) complete: saved (m12); cleared and bound old -> new; resume prompt m15 delivered.` If the clear retry window expires, the note includes the last refusal. `context_line` also moves the line note for a seat that is off.
- **`"idle"`:** when the seat has been idle for `idle_minutes` and nothing it waits for will come before `cache_minutes` are over, it is asked to save while its cache is still warm, and the handover gives up when they are over. `atmd` has no cache life of its own, because none has been measured: without `cache_minutes` there is no idle rule. What a seat waits for is read from tasks and decisions only: a task it holds or has given to another agent, or a pending decision it asked for (your answer counts as coming in time). A seat that works by messages alone waits for nothing as far as `atmd` can see, so use `"idle"` only where tasks carry the work.
- **The file is read at every turn end,** in the project's main checkout, as `atm me` reads it. An edit takes effect at the seat's next turn end with no restart and no respawn, and removing `auto_handover` stops it. A missing or invalid role file is off.
- **It never does more than the command would.** Nothing is typed while a turn runs, over a draft, at a dialog or into the pane you are focused on: the request waits there as any message does, and after ten minutes the manager is told. Nothing is cleared unless the agent saved. A session that shows background work is left alone, and so is one that has done nothing but start.
- **One try per session.** Whatever becomes of it, `atmd` asks no second time in that session; the next session has a try of its own. If the start-line turn already ends at or above its line, the note says that this session will not be handed over automatically: neither the context nor the idle rule runs in it, even after later work. A handover that could not start (the agent is not ready, another handover of it runs) sends nothing and does not count.
- **A session whose cache has run out is not cleared.** With `"idle"` on, a seat idle for longer than `cache_minutes` is left as it is, and gets the note at its line. Clear it with a command.
- **Limits.** All of it is in memory: after a restart of `atmd` nothing is done until a turn ends, and a handover the restart cut off is lost with its note. A request that was never delivered when the handover gave up stays queued, as for the command: once what held it is gone it is typed, the agent saves, and nothing clears the session. For an idle seat that late turn runs on a cold cache; `atm msg give-up mN` cancels the request. An older `atmd` refuses a role file that has these keys (`agent up` fails and `atm me` reports an invalid role), so remove them before going back to one.
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

### Organisation overview (`atm org`)

```sh
atm org                         # needs-you summary, projects, seats and open work
atm org --format json            # original daemon reply, including additive fields
```

The first line is owner triage: deduplicated attention items, unread owner messages, unnamed sessions needing attention, and when the owner went away. Item rows identify pending/provisional decisions, stuck or attention-needed seats, failed deliveries and broken reporting lines. These counts are separate signals, not numbers to add together.

Each project shows its recorded seats, including down/stuck ones, nested under their managers. A cross-project manager is a reference such as `-> gamma/chief`, not a moved or duplicated seat. Broken lines show their error without hiding other seats. If reporting is unavailable, one note explains why and all seats remain in a flat list. Each seat shows state, harness, context percentage (or `-` when unavailable) and unfinished task IDs; open task summaries and pending/provisional decision counts follow. Open tasks belong to their recorded project, falling back to the current assignee's project then the agent creator's project for legacy tasks; unassigned legacy tasks created by owner/system appear under **Unplaced tasks**. A project can be a non-repository folder; role-only seats that have never registered are not listed.

The view adds no visibility and is the same for named and unnamed callers: only metadata already available through agent/task lists and global owner/decision counts, never private decision titles/context/files or message bodies/attachments. It makes one `org.tree` request, not client-side joins or local role/Git/tmux lookups. The daemon takes one fold snapshot with its `seq`, one live registry read and one role-file pass; they are **not one atomic instant**. `seq` is the fold's replay/commit cursor, not a version of the registry or files. At most 100 attention items and 50 open task summaries per project are listed; `+N more` preserves the totals when those caps are hit. The complete response line, including its envelope, request id and newline, is limited to 512 KiB; an oversized reply is refused, not silently cut off.

The current daemon advertises `org.tree` with a 12-second request budget. Its role pass shares the retained reader lane used for task, decision and inbox manager authority: each recorded role is read once, limited to 256 KiB, with five seconds total for admission and the whole pass. A busy/timed-out reader returns `reporting.state = "unavailable"` with every seat flat (managers null), while preserving tasks, counts and other owner signals; configuration failures mark only affected reporting lines. Infrastructure failures still fail the read. Roles are reread on the next call; no cross-call cache is used.

JSON preserves the daemon's raw reply, including nulls and unknown additive fields (such as future department records); consumers must ignore fields they do not know. Text escapes terminal controls. A daemon that does not advertise `org.tree` refuses with an upgrade/restart diagnostic; there is no local fallback.

`atm setup` writes **prefix then `O`** (uppercase) to its generated `tmux-bindings.conf`, alongside `C-s` for the sidebar popup. Source that file from your `.tmux.conf` using the path setup prints (by default):

```tmux
source-file ~/.config/atm/tmux-bindings.conf
```

Prefix-O opens `atm org` in a 90%-wide, 90%-high popup. This requires `less`; scroll to read and press `q` to close. The `C-s` sidebar popup is unchanged.

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

Except for JSON-only `msg history`, these commands accept `--format text|json` (default `text`). Send requires `--to` and `--body`; optional flags are `--kind`, repeatable `--attach`, `--task`, and `--project-root`. Kinds are `note` (default), `brief`, `report`, `question`, `answer`, and `review`. Inbox accepts `--for`, `--project-root`, `--unread`, `--id`, `--after`, and `--limit`; `--id` and `--after` are mutually exclusive. IDs use canonical `mN` form.

- Targets are `owner`, a full `project/name` address, or an exact bare agent name. Full addresses bypass name lookup. Bare names use `--project-root` if supplied (relative paths become absolute); otherwise they use the uniquely bound, non-Down caller pane's project, or the absolute current directory if unbound. Unknown/ambiguous names or caller bindings fail rather than guessing across projects. `system` is not a recipient or inbox.
- If a Ready agent records a real session id but its pane has a non-excluded top-level `pending-<pid>` row and no matching recorded real session, `msg send` refuses with **message not queued** before creating a message or typing anything. A pending row means process discovery has not learned the harness's session identity; it does not mean the harness is busy. Inspect `atm list` and `atm agent list` before retrying. A genuine harness event can restore the identity. Pi-atm also retries its connection without another pi event and, after queued frames drain, can send one measured-idle identity snapshot per connection. This needs known idle, no pending messages, a real session id and no pending interactive input or open native/select dialog; it is not a heartbeat or a replay guarantee. Inspect the pane before deciding whether to start a fresh session. This refusal does not cancel older queued messages; Ready agents without a recorded session (lazy first prompts) and other lifecycle states keep ordinary queue policy. A successful `queued` result still is not delivery confirmation.
- Sender attribution comes from the caller's pane binding; an unbound caller is Owner. There is no `--from` or send-now bypass. Owner can inspect agent inboxes; agents receive full content only for their own inbox and delivery metadata for their verified reporting descendants. System notices and Owner-only resolution rely on local socket/client-supplied pane attribution, **not authentication against hostile local clients**.
- Body limit: 16 KiB of UTF-8 bytes, with no trimming. An empty body needs at least one attachment. At most 32 attachments, each a nonempty literal path of at most 4096 bytes; task IDs are at most 256 bytes. Prefer absolute attachment paths across worktrees. Neither CLI nor daemon uploads/reads attached files, checks their existence, or automatically reads stdin. Quote multiline shell arguments to preserve blank lines and Unicode.
- Every agent delivery uses the one-line reference defined by `message_delivery::inbox_reference`: for `m123`, `[m123] Read: atm inbox --id m123`, with no trailing newline. The one exception is the daemon's start line (see [Starting a named agent](#starting-a-named-agent)), typed as its own text, `[m123] New session: run the shell command atm me`. The body, task and attachments stay intact in the inbox. **Delivered means the harness accepted the retrieval reference**, not that it read the body or did the work. Owner messages are delivered directly to the inbox without terminal input.
- Owner/self inbox reads include full bodies, metadata, durable status and projected hold/retry/predecessor fields. Text shows a `delivery: <phase>` line with pane, attempt, blocking predecessor (`blocked_by`), retry time (`retry_at`) and reason only when present; JSON retains the complete delivery diagnostics. Text indents every body line by two spaces under its entry, preserving body backslashes, line breaks and tabs and escaping other control characters. Prefer `--id` for individual reads; `--format json` is the exact form, preserving original content, nulls and additive daemon fields (DEL/C1 are JSON Unicode escapes). Limit is 1–1000 (default 100), even with `--id`; pages are bounded to 512 KiB without truncating bodies. An oversized legacy entry is refused with its ID. `next_after` indicates more matching messages. Unread counts cover all statuses, independent of page/filter.
- Fetching never acknowledges reading. Explicit `atm msg ack` takes 1–1000 distinct IDs from your inbox; the whole batch must exist and be Delivered. A wrong recipient or undelivered entry refuses the whole batch. Already-read IDs are idempotent; output counts only newly marked messages. If fetch precedes the delivery receipt, wait for Delivered before acknowledging, not by sending again.
- Retry is Owner-only for a latched/held delivery and **keeps the same ID and increasing attempt count**. Inspect the pane first: retry can duplicate delivery, hence the required `--accept-duplicate-risk`. It does not reset the attempt limit or bypass safety guards. After another restart an unconfirmed attempted message is held again. Give-up is allowed for held Queued/Delivering messages, not just one Blocked reason; repeating it on Failed is idempotent. A late receipt can still change a given-up message to Delivered, and cancels a pending same-ID retry. Already Delivered messages cannot be retried or given up.
- **Give-up does not clear the editor.** Its successful reply names the source's canonical inbox reference (or start line) as a **possible** leftover: text adds a `warning:` line, and JSON adds `editor_warning`. This advisory also appears for never-attempted messages and repeated give-up; it is not a fresh observation, proof of ATM authorship or proof that the reference was never submitted. Older daemon replies may omit it. Inspect the pane before editing it: releasing the message's FIFO hold does not empty an editor, so a follower can remain draft-blocked. A token-only “Enter was not sent” diagnostic likewise does not prove staging in the active editor. Give-up sends no editor input, and a late receipt still confirms delivery.
- **Lifetime delivery notices:** owner notices deduplicate by source message and semantic reason for that message's lifetime, across retry and daemon restart. Focus notices stay in the inbox without external notification. Recognized final-failure notices can be reconstructed; a transient observation that vanished before its first notice committed cannot be recovered after a crash.
- **Retained recovery proof:** input/outcome facts for a held draft are process-local. A diagnostic, a visible token or a daemon restart never proves that ATM staged the active draft safely or that Enter was not sent.
- **Guarded delivery recovery:** an original attempt retained as having pasted its reference but provably sent no Enter may receive at most one freshly guarded Enter, under the same message ID and attempt. There is no repaste, new attempt or new confirmation window. Unknown input/focus/identity, a changed or ambiguous draft, a dialog or a running turn still holds delivery. This is not an automatic Owner retry or editor cleanup; the input-outcome proof does not survive daemon restart. A late exact receipt can still confirm the original delivery.
- A name-lookup timeout means no operation was sent. Once an operation request is sent, a timeout is outcome-unknown, not rollback. Inspect inbox/pane before deciding what to do; the CLI never automatically resubmits. Ordinary message calls allow 15 seconds (10-second router budget + 5 margin); retry/give-up allow 70 seconds (60 + 10); name lookup allows 20 seconds per `agent.list` call (10 + 10).

A strict reporting ancestor can inspect a descendant with `atm inbox --for alpha/worker`, including `--id`, unread filters and paging. The existing JSON shape retains IDs, sender/recipient/kind, sent/read timestamps, status, delivery phase, attempt, recorded pane/session, retry time and FIFO predecessor. The manager view sets body to `""`, attachments to `[]`, and task/reason/confirmation to `null` **before page byte accounting**; blank/null means **withheld for a manager, not empty stored content**. No preview, attachment references/count or free-text diagnostic is exposed, and no attachment file is read. Owner/self content is unchanged. Descendant unread counts are available to verified managers; Owner's count-only exception stays global. Fetch grants no ack/retry/give-up authority. Reporting grants use one captured committed snapshot and current role files, never a later recipient/root/binding; file edits affect subsequent admissions, not already admitted reads.

`atm status` requests only Owner's aggregate unread count, even from an agent pane: it appends `N unread` when nonzero and leaves the existing summary unchanged at zero. An old/unavailable daemon or invalid aggregate response produces `? unread` plus a stderr diagnostic, not a false zero or any Owner bodies. Command errors exit 1; invalid command syntax exits 2.

### Terminal chat

```sh
atm chat --thread alpha/chief
```

Chat is a separate terminal screen and connection, not a Sessions tab. Use a full
`project/name` address; chat never guesses a project from a bare name or folder.
Without `--thread`, it starts at **For you** when unread messages exist, otherwise
the first named Direct seat, otherwise read-only **All**. A bound caller refuses
startup with **chat runs from an unbound pane**; there is no seat-view fallback.
The pane is checked again before every send/read acknowledgment. That check and
the mutation are separate RPCs, not an atomic guarantee against concurrent adoption.

After `atm setup`, the generated **prefix C-t** binding creates/selects the window
named `chat`, flips to the previous window from chat, and revisits the same running
chat. Source the generated bindings yourself; setup does not change a running
server's configuration. Reserve `chat` in each session: a pre-existing unrelated
window with that name is selected, never replaced. With no previous window,
`last-window` can fail visibly without closing chat. Popup chat is not enabled.

Messages run oldest at the top to newest at the bottom, with the composer below
and a **new** divider before the first unread incoming Owner message. For you
shows the global unread count but loads only the **first page** (100 rows);
per-sender counts describe that page, not all unread messages. A capped decision
summary is a lower bound. Use Direct/All and older pages for the remaining history.
Queued/delivering, held (with live reason), delivered, **seen (read)** and failed
are distinct. Delivery confirms acceptance of the inbox reference, not execution;
history alone has no live scheduler hold diagnostics.

| Mode | Keys |
|---|---|
| Normal | `j/k` move; `gg` oldest / `G` newest; `Ctrl-d/u` scroll wrapped body lines |
| Normal | `h/l` or `Tab` columns; `J/K` unread threads; Enter opens a thread |
| Normal | `i` compose; `r` reply; `/` search; `n/N` next/previous literal case-sensitive match |
| Normal | `y` copy the original body to the tmux buffer; `?` help; `q` / `Ctrl-c` close |
| Insert | Enter sends; `Ctrl-j` adds a newline; Esc keeps the draft |
| Search | Enter applies (1–256 UTF-8 bytes, empty clears); Esc cancels |

Esc never closes chat: it leaves Insert/Search/Help and does nothing in Normal.

Seen means a durable read acknowledgment, not merely a fetch or list preview.
Only delivered unread messages **to Owner** qualify, after their final wrapped
body line is drawn and tmux reports known-on `focus-events`, a non-control client's
exact `focused` flag, the active chat pane, and `pane_in_mode` equal to zero.
Copy mode and other pane modes block read acknowledgments. A clipped 16 KiB body
does not qualify just because its beginning appeared. Unfocused/locked, focus-events
off or unknown, failed focus/mode checks, and no tmux leave messages unread with a hint.
Known limit on tmux 3.6a and 3.8: **an open popup over the chat window still counts
as seen**; neither version exposes a usable popup indicator in these observations.
Every unbound pane shares the same Owner inbox: chat's read marks affect other
Owner-side readers too. Reliable application/lock focus reports depend on the
terminal; headless tests and supplied focus escapes do not verify a compositor.

A send timeout, disconnect or uncertain daemon error may still commit. Chat never
automatically resubmits: it locks the original draft and reconciles committed rows.
Only explicit **Ctrl-r**, at least ten seconds after dispatch, unlocks editing,
always warning **send may still arrive**. Unlocking does not send or prove failure;
resending risks duplicates, and identical messages from another Owner pane are
ambiguous. An explicit no-send refusal preserves an editable draft. Reconnect
retains drafts/history; a disjoint new head resets the cached window and says
**history reloaded**. Missing daemon capabilities/watch disable mutations; ledger
replacement requires an explicit restart, not an automatic cursor rewind.

Bodies retain the 16 KiB UTF-8 limit; controls are displayed safely. Copy needs
tmux and copies literal body bytes, not an OS clipboard. Attachment paths are shown
as references, never opened. Down/stuck seats and stale diagnostics are explicit;
unknown/gone targets cannot send. A turn-ending observation covers only chat's
running interval and is not proof that a specific request was ignored. Replies
must be sent as durable messages: there is no transcript mirroring, pane jump,
room, picker or decision action in this slice.

### Message history and search

```sh
atm msg history --agent alpha/worker --correspondent beta/lead --text 'review'
atm msg history --agent alpha/worker --limit 100 --cursor '<next_cursor>'
```

History prints JSON only (`--format json` is optional), makes exactly one page request and never resolves bare names or follows a cursor automatically. It accepts the eight params below as matching flags, with a 17-second total budget including startup/transport.

The protocol 1.1 `msg.history` read method takes object/null params: optional full `agent` ID, `task`, `correspondent` address, UTC RFC3339 `since`/`until`, literal `text`, `limit` and opaque `cursor`. For example, `{"agent":"alpha/worker","correspondent":"beta/lead","text":"review","limit":100}`. All filters are ANDed: task is exact, text is a case-sensitive substring of the body only (never attachment contents/references), and sent time is `[since, until)`. Text must contain 1–256 UTF-8 bytes, task at most 256; limit is 1–1000 (default 100). Unknown params, inverted times and invalid/mismatched cursors are refused as `invalid_params`.

Owner defaults to all messages, or an agent's incoming **and outgoing** conversations when selected. A named caller defaults to its own conversations, including self-sends once; selecting itself explicitly is identical. A strict current reporting ancestor selecting a different agent gets **only that recipient's inbox delivery metadata**, never its outgoing conversations. This uses the same full-chain role admission and redaction as inbox reads, repeated on every page; task/text filters are refused in delivery-only scope even on an empty dataset. Correspondent selects either endpoint for Owner/all, the other endpoint of a selected conversation, or the sender for delivery-only reads. Grants give no mutation authority.

The result is `{"seq":42,"snapshot_seq":40,"visibility":"full","messages":[],"next_cursor":null}` (`visibility` is `full` or `delivery`). Rows use the existing inbox shape. Order is newest **numeric send sequence** first, not wall time. Pass `next_cursor` with the same filters/reader/scope for another page; it is at most 4096 bytes and is not authority. The first page's committed `snapshot_seq` fences membership, excluding later sends across inserts/restart on the same ledger; `seq` and delivery/read facts are current for each page, not historical as-of-fence. Limit may change between pages. No total/unread count, event, ack, delivery work or attachment-file read is produced. Delivery shows only committed attempts/receipts/status, not scheduler progress (`retry_at`/`blocked_by` are null); neither receipt nor ack proves body reading/execution.

The serialized result, including cursor/framing **after redaction**, is at most 512 KiB. An oversized first row is refused, never truncated/skipped; byte/count-limited pages resume after the last emitted row. The complete wire line, including request ID/envelope/refusals, is bounded to 1 MiB. The router allows 12 seconds (one atomic fold/sequence snapshot and current-role admission); history needs the organisation service, not a functioning delivery runtime. Limits: retained messages and snapshot clones/linear scans remain unbounded in total, with no index or compaction; cursors do not handle ledger replacement/rollback. Pane attribution is **not authentication**, and existing `log.tail`/watch paths expose ledger bodies outside this method's scope: this is not daemon-wide secrecy. Use the [live inbox delivery diagnostics](#confirmed-messages-and-inbox) for current scheduler holds rather than inferring them from history.

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

## Subscription usage

`atm usage` is an owner read, separate from [task usage](#task-usage) and
context-window usage. It shows the daemon's most recent subscription reading
from a bound Claude Code seat. It uses the existing Claude status line and does
not need a hook reinstall, network query, or credentials. Run it outside a
named agent pane. This follows ATM's local owner attribution convention, not
an authentication boundary.

No usage configuration is needed for one account per meter. All bound Claude
seats with a proven current parent session feed the same local profile,
`claude`. These are readings of one shared meter, not totals across seats or
proof that their accounts match. With multiple accounts, put one alias per
account in ATM's `config.toml` (`$XDG_CONFIG_HOME/atm/config.toml`, normally
`~/.config/atm/config.toml`):

```toml
[[usage.sources]]
agent = "project/builder"
harness = "claude_code"
meter = "claude"
profile = "work"

[[usage.sources]]
agent = "project/reviewer"
harness = "claude_code"
meter = "claude"
profile = "personal"
```

Use a full bound `project/name`. A profile is a local alias of 1–32 lowercase
ASCII letters, digits, underscores or hyphens, starting with a letter or
digit. Share it only for seats you know use the same subscription. Update the
mapping when a seat changes accounts and restart `atmd`. Explicit sources
replace the default for their meter only: unlisted seats for that meter are
ignored. Invalid or duplicate source settings disable usage collection instead
of falling back to a shared profile; other daemon features keep their own
configuration. Currently only the `claude_code`/`claude` pair reports usage.

`usage.list` takes `{}`. It returns an array sorted by `(profile, meter)`,
also emitted by `atm usage --format json`. Each entry includes the local
`profile`, `meter`, `source` harness, UTC `observed_at` (daemon receipt),
`stale`, and `windows`. Windows are sorted by `(scope, duration_minutes)`:

```json
[
  {
    "profile": "claude",
    "meter": "claude",
    "source": "claude_code",
    "observed_at": "2026-11-19T23:42:19Z",
    "stale": false,
    "windows": [{"scope": null, "duration_minutes": 10080, "used_percent": 52.0, "resets_at": "2026-11-22T21:00:00Z"}]
  }
]
```

`scope: null` means the whole meter; `resets_at: null` means the reset was
not reported. Claude may report 5-hour or 7-day windows or both. A missing
window was not reported in the latest valid sample, not 0% used. The daemon
keeps the last nonempty valid sample in memory, and identical status redraws
do not refresh its timestamp. A sample is stale after 60 minutes or when any
reported window reaches its reset. `[]` means no sample received, including
after a daemon restart until a new push. An old daemon that does not advertise
`usage.list` returns a CLI error, not `[]`.

## Evaluation report

On a daemon advertising `org.report`, `atm report` reads one evaluation projection; it never joins local task/session files or falls back to other methods. A daemon without that capability is refused with the usual restart message. This reporting-line, observed-accounting view is distinct from the frozen, ranked [owner return digest](#away).

```sh
atm report
atm report --project alpha --since 7d --until 1h --bucket 5
atm report --since 2026-01-01T00:00:00Z -f json
```

`--since` and `--until` accept RFC3339 or checked `Nh`/`Nd` durations before one client timestamp. The default window is all time through now. Tasks created before `until` are included unless they ended before `since`; their outcomes and numbers are current **whole-task totals**, not clipped to that window. Start means first assignment; end means acceptance or cancellation, not approval of a plan. Unknown state is a separate outcome. A task's recorded project wins; legacy tasks without one use their current or last assignee's project (retained through reopening), otherwise their agent creator's project, otherwise **unplaced**.

Text gives whole-scope totals, per-project counts/decisions/trends, then the returned task rows. JSON preserves the daemon's entire reply, including additive fields. The default task page is 100; `next_after` explicitly marks more rows, while project blocks always cover the full scope. The read-only `org.report` parameters are `project_id`, `since`, `until`, `bucket` (default 5), `after` and `limit` (default 100); unknown keys are refused. Bucket and limit are 1–1000, a cursor must name a canonical existing task, and a complete reply exceeding 512 KiB is refused, never cut to fit.

Turns, state seconds, review rounds, rework, peak context and exact microdollar cost come from the **same ledger as task.get**, including accepted frozen records. Cost is a **lower bound**: the first session observation is a baseline, and spend while an agent held no task or several belongs to none. `null` is unknown, never zero. Known cost is labelled **harness-reported**, with contributing session harness/model facts when summaries exist; no price is estimated. Context at submission is the last valid observation before the **first result** submission, not the latest resubmission. A turn counts an observed parent prompt regardless of whether it was typed or delivered; subagent prompts do not count.

Owner step-ins count verdicts (not automatic review closure), redirects (assignment/state/review/attachment, except first assignment of an owner-created task), decision answers, untagged messages to a seat holding exactly one unfinished task, and redo. A task's answer links use `rests_on` or its requester's unique unfinished task when the request was recorded. **Project decision counts instead use requester visibility and request time in [since, until), including unlinked decisions**, before task paging; owner/system requesters are unplaced. Designed owner verdicts can be distinguished using each JSON row's measure. Tagged owner messages are excluded because generated task notices cannot yet be distinguished from hand-written ones; pane typing/raw sends are not measured. Recorded `redo_tasks` increments each affected row's `redone` and redo step-in once per overrule; project `tasks_redone` counts those invalidations by visible requester, not accepted-again completions. Unannotated legacy overrules add no recorded redo. An overrule matching both answer and redo increments both kind counters but only once in the step-in total.

[Spoken-answer relays](#spoken-answer-relay) have their own `relayed` counter, including self-relays; system takes and agent-decided counts are unchanged. Only your later confirmation or overrule counts as your answer.

Change trends use accepted `work` tasks ordered by acceptance, newest-anchored complete buckets, and lower medians of turns/context/reviews/rework and known costs (with their known count). The incomplete oldest bucket is dropped; directions need two comparable buckets. They are **not size- or area-adjusted**. Budget overage is positive turns/cost above a known limit and seconds past deadline through completion or now, each floored at zero: an **upper bound** on what stopping might have saved, not a stop rule. Missing limits/measurements stay unknown; no budget means no overage record. Tokens, prompt origin, findings, diff size/area and a composite score are not reported.

The reporting-line read policy is owner (an unbound caller) versus a named reader's self and direct/indirect reports: task creation or current/last assignment grants a row within that set. Project aggregates include only those rows, and decisions use visible requesters. Configuration-broken chains omit those descendants, as decision.list does; infrastructure failures fail a named read instead of yielding a partial report. Owner reads never consult role files. Reports share the task/decision/inbox reader slot; role files are fresh per admission, not an atomic multi-file snapshot. The cursor and org state come from one committed actor turn, including skipped future events. The router allows 22 seconds and the CLI 27 seconds, matching decision reads. No decision titles, message bodies, verdict notes or prompt text appear in a row. This remains single-user attribution, not authentication.

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
# After the creator/manager/owner accepts the plan, the task is working again.
atm task submit t41 --file '{"path":"report.md","kind":"report","note":null}'
```

The default submission is `result`. Creator, reporting-line manager or owner can request changes or accept, subject to the measure; nobody can give a verdict on their own assigned task:

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

If a task is `changes_requested` and its assigned seat is `Down`, the assignee's strict reporting-line manager or the organisation owner can accept it from a reviewed fix:

```sh
atm task carry-back t41 --from-fix t56 --review-task t60 --note 'The fix covers the remaining case'
```

The distinct, same-project fix must still be accepted, with that review used to accept its **result**, and the review must still be accepted. A parent link is neither required nor sufficient. The caller records why the fix covers the original. This does not reassign the original, change the fix or review, or waive an owner-only measure or outstanding decision redo. The original records the actual caller and evidence in its verdict. Its creator gets a Note (System routes to Owner; no self-note), not a request to resume work. Inspect the task after an uncertain reply; repeating the operation once it is accepted is refused without another write.

### Task commands and data

Every subcommand accepts `--format text|json` (default text); the option also works before the subcommand, e.g. `atm task --format json get t41`. JSON preserves the complete original daemon result, including additive fields and newer enum strings. Text escapes terminal controls; `get` shows every task field, including the measure, budget, decision reference, files, timestamps and cost record when present. Newer enum values read as `unknown` in summaries rather than making reads fail; full raw get fields and JSON remain visible. This daemon refuses unknown enum inputs.

| Command | Operands and optional flags |
|---|---|
| `create <title>` | Required `--measure <JSON>`; `--project <id>`, `--kind <kind>` (default `work`), `--parent <id>`, `--judges <id>`, `--budget <JSON>`, repeatable `--file <JSON>`, `--rests-on <decision-id>` |
| `assign <id> <project/name>` | Queues the task's brief to its assigned agent |
| `set-state <id> <state>` | `--reason <text>`; directly settable states are `open`, `working`, `waiting`, `blocked`, `cancelled` |
| `submit <id>` | `--what plan\|result` (default `result`), repeatable `--file <JSON>` |
| `attach <id> <path>` | Required `--kind <file-kind>`; `--note <text>` |
| `review <id> <review-task-id>` | Starts an existing review of the submitted task |
| `verdict <id> <accepted\|changes_requested>` | `--review-task <id>`, `--note <text>` |
| `carry-back <id>` | Required `--from-fix <id>`, `--review-task <id>`, `--note <text>` |
| `list` | `--project <id>`, `--agent <project/name>`, `--creator <owner\|system\|project/name>`, `--state <state>`, `--kind <kind>`, `--parent <id>`, `--after <id>`, `--open`, `--limit <1..1000>` (default 100) |
| `get <id>` | Complete task, not just its list summary |

Agent ids must be full `project/name` addresses for these commands: there is no bare-name lookup, implicit project filter or `--project-root`. `list` includes closed tasks unless `--open` excludes accepted/cancelled tasks. All filters intersect. A `next_after` JSON cursor or `next: --after tN` text line means another matching page exists; repeat the command with that cursor and the same filters. Pages/get are bounded to 512 KiB and are never silently truncated.

Kinds are `work`, `review`, `plan`, `errand`, `drift_check`. The ten states are `open`, `assigned`, `working`, `waiting`, `blocked`, `submitted`, `in_review`, `accepted`, `changes_requested`, `cancelled`. Assign/submit/review/verdict have dedicated commands rather than direct state changes. Waiting and blocked require a reason. Creator/reporting-line manager/owner can reopen eligible tasks with `set-state ... open` or cancel them with `set-state ... cancelled`; accepted/cancelled tasks are final.

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

### Task projects and pieces of work

`create --project alpha` records the intended registered project independently of the worker. Without it, a named caller uses its own project; an owner uses the sole registered project, must select when there are several, and may create unplaced legacy work when none is registered. A named caller may select another project only if it is a **strict ancestor of at least one registered agent** there, through a fully validated current reporting chain; down seats count. No special manager name or department membership grants authority. A target with no registered agents refuses named cross-project callers; owner and same-project creation need no role-file read. Only the selected project's intent measures apply, never the creator's home intent.

```sh
# From gamma/chief, whose current reporting line includes an alpha agent:
atm task create 'Alpha work' --project alpha --measure '{"kind":"mechanical","command":"alpha-check"}'
atm task assign t41 beta/worker
atm task list --project alpha --open
```

The recorded project never changes on assignment, reopening, decision redo or cancellation. `get` exposes `project_id`; list summaries stay unchanged. `list --project` filters before pagination and intersects with every other filter. Unknown projects are `not_found`. Old events lacking the field stay unrecorded: list/tree use current assignee then agent creator, while reports retain current-or-last assignee then agent creator; otherwise work is unplaced. Replay consults no current intent/role files.

A piece of work is the existing parent-linked task tree, anchored by its top ancestor; an independent root is its own piece. `--parent` delegates within that tree, and `--judges` remains the independent review relation. There is no additional work record or window/tmux coupling, and links do not override a task's project.

**Downgrade limit:** older builds ignore the additive `task.created.project_id` field and lose its attribution/admission semantics; older `task.list` rejects the project selector. This is not semantic rollback safety: do not permit cross-project creates on a downgraded daemon. Retain raw logs; no migration or log rewrite is required.

### Task identity and notification outcomes

The daemon derives the caller from its pane, not its working directory or `ATM_AGENT`. A named member may work/wait/block/submit only its assigned task. The task's creator, owner, or a strict manager above its **current assigned agent** can assign/manage/review/give verdicts; a name such as `lead` grants nothing by itself. An unassigned task can be assigned only by its creator or owner, not by the proposed recipient's manager. A lead holding a chief-created task cannot reassign that task to its report: delegation means creating a separate task with `--parent` and assigning that child using creator authority. A task's assigned agent cannot approve itself, and the measure can further restrict acceptance. An unbound caller counts as owner, just as for messages. There is no `--from`, `--by` or authority bypass; pane attribution is not authentication against hostile local clients. Anyone may create a task or attach a file, subject to daemon validation.

Reporting lines come from current role files for started and adopted seats alike, never from the saved role at `agent up`. Bare managers are relative to each node's project; full IDs cross projects. For example, `alpha/worker → alpha/lead → gamma/chief` permits the chief to manage alpha's assigned work but gives an unrelated `beta/lead` no authority; gamma may be a non-repository project folder. Project count and seat names are configuration, not built-in policy. Missing roles/managers end at owner. The whole reached chain must validate before any manager grant: invalid roles/managers, unknown agents or cycles refuse with the broken node's ID, never role contents. Owner/creator authority and own-agent work/submission do not read roles. A down intermediate manager does not erase the line.

**Authority limits:** files are read per admission, not as an atomic multi-role snapshot; edits affect later admissions and do not revoke already admitted work. A grant is scoped to the assignee ID and is refused if the writer sees a different assignee. Task, decision, inbox and report methods share one retained detached-reader slot per server; a hung role read denies manager-derived work until it clears, without blocking owner/creator task authority, owner/self inbox reads, owner/own decision get, or global count-only reads. A mixed normal decision list can still need the reader and time out; infrastructure failure never returns a successful partial union. Within the single-user threat model, a non-ancestor querying a seat with a broken reporting chain may see its diagnostic, including an unknown manager ID or project root. This does not change handover/reminder note routing, which can still use a saved role.

Assignment queues a task-correlated **brief** to the assigned agent, with only brief-kind files attached. Submission queues a **report** to the creator with the submission's files. Automatic briefs/reports addressed to their own named sender are omitted: self-assignment or self-submission still commits with `changed: true, notification: null`, without starting another turn. This does not suppress manual `msg.send`, change other recipients, or remove historical messages. A non-final verdict queues an **answer** to the current assignee, signed by the verdict actor: accepted plan while the task is still working, or changes requested while still in that state. Answers have no attachments; read the current verdict note/files with `task get`, and follow the answer's review-task reference when present. Accepted results and secondary review closure send no resume answer. These use the normal guarded, confirmed-message runtime; `queued mN` is **not delivery confirmation**. Read and acknowledge the resulting inbox message normally. An unchanged repeated operation is shown as `unchanged` and does not enqueue another message.

Cancellation queues an actor-signed **note** to the retained holder; reassignment/reopen queues one to the actual former holder. Another actor's revocation of assigned work also tells its creator (System routes to Owner). Taking a submitted task back to working invalidates its old report with a creator Note; ordinary work starts do not. When a parent leaves review, its previously linked, still-held live reviewer gets a Note referencing both tasks, even if the parent was Accepted. The review task is not automatically cancelled/closed, and submitted/final/unknown/unassigned or unrelated reviews stay quiet. Notes have no attachments and refer to current state rather than copying reasons/files. Actor/self Notes and duplicate (recipient, task) sends are omitted.

All these messages use ordinary guarded inbox references. They **do not interrupt a running turn**, bypass focus/drafts, clear a seat, or promise immediate stand-down. Before continuing work, compare `task get`'s `agent_id` with `atm me` and inspect state: stop obsolete work if final or no longer yours. A reviewer also reads its judged task. Urgent stand-down requires separate authorized coordination. Do not duplicate automatic notices; send explicit questions for actionable waiting/blocked work, and explicit messages for actionable late attachments or unlinked-review changes, which do not automatically wake anyone.

Invalidation uses an atomic actor before-image for lost ownership/review context, but coalesces against a fresh post-state: regained ownership, a restored current review, or a new submission suppress their respective loss/withdrawal Notes. Same-state cycles and changes after that snapshot remain limits. Real-harness stopping/resumption and skill compliance remain unverified.

Verdict answers use a fresh post-commit snapshot, not an exact-at-verdict receipt. If a newer state is submitted/in_review, waiting/blocked, assigned/open, final or unknown, the answer is suppressed (`notification: null`, not an enqueue failure). Same-state or reassignment cycles cannot be distinguished and may produce a stale answer: always re-read the current task before resuming, without withdrawing a newer submission or reopening final work. Do not also send a manual answer; conditional repair is appropriate only after investigating a missing enqueue, not a held delivery or intentional suppression. A named lead needs authorized diagnostics to inspect another assignee's inbox.

A mutation attempts at most three automatic sends, ordered legacy Brief/Report/Answer, former holder, creator, linked live reviewer (not all apply together). The existing single `queued mN` result means all planned enqueues were confirmed and names the primary message, otherwise the first Note. If any send fails, remaining sends are still attempted: `unconfirmed` retains the first failed send's error code and lists all failed audiences and known queued IDs. No rollback, blind retry or per-recipient wire fields are added.

A task mutation and its automatic message enqueues are separate durable operations. If the task committed but any enqueue was not confirmed, the successful response includes `notification: unconfirmed` with the daemon's diagnostic; text displays it prominently and JSON retains it. Inspect the task and inbox before resending. A daemon death between those writes can leave the task without its message, and replay does not re-emit old notifications. The CLI does not repair that gap by retrying. A notification status introduced by a newer daemon also stays a successful committed task: text shows the escaped status and explains that this build does not understand it. JSON passes the original result through without requiring the text renderer to understand its shape or notification vocabulary.

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

`task.create` reads the selected target project's intent **on every call**, before committing a task. The typed measure must match a listed entry exactly: a mechanical command, a reviewed rubric path, or `{"kind":"owner"}`. Outside whitespace/backtick delimiters are removed; internal command bytes are not normalized. An owner caller is subject to the same measure list. Commands/rubrics are not executed or evaluated by this check; the existing task verdict rules still decide who may accept work. A refused create adds no task or event, and changes to the current intent do not invalidate replay of existing tasks.

A missing target intent file preserves legacy unchecked creation. A **present** empty/malformed file or missing/empty measures section refuses creation, including for the owner; it does not disable validation. A home intent without measures still refuses home tasks but does not block an authorized task explicitly targeting another project with a suitable measure. Intent reads are bounded to 256 KiB and 5 seconds; nonregular/symlink policy paths, invalid UTF-8, oversize files and read failures are explicit errors. No intent directory/file is created while checking policy.

The additive optional `project_id` parameter (`atm task create --project`) selects a **registered** project. Named callers default to their own project and may target another only through the reporting-line rule above; an owner may select any registered project. For an owner with no selector, a sole registered project is used; multiple projects require `project_id`, and no registered projects preserve legacy unchecked creation. An unknown selected id is `not_found`. The admitted project and creator are bound to the same authority/policy snapshot, and the project is recorded in `task.created`; no cwd, department or window inference is used. File reads are admission-time checks, not atomic or revocable authority.

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

An agent pane sees its own requests and those of its direct or transitive reporting descendants; an unbound owner caller sees all. Verified managers can list/get complete decision entries, including context, files and answers, but cannot resolve or withdraw a report's request. Current role files determine the reporting line across registered projects, including adopted seats and non-repository project folders; the entire reached chain must validate to owner before a grant. A broken requester chain contributes no manager-visible rows to a list without hiding own or other valid rows; a direct get refuses with the broken agent ID, never role text. Reader infrastructure failures fail the read rather than returning partial counts. Owner reads, own get and global count-only reads bypass role lookup. Pending and provisional decisions remain on the default list, with separate `pending_count` and `provisional_count` totals. Counts cover the union of authorized requesters, not just the page or status filter. `atm status` independently asks for the global **Pending-only** count, even from an agent pane: nonzero adds `N decisions`, zero adds nothing, and missing/unavailable/bad aggregate data adds `? decisions` plus a diagnostic without hiding session or unread counts. A provisional decision is not counted as pending; it remains in `atm decisions`.

Only the owner records a final resolution. A pane bound to an agent is refused for final resolution; the [designated relay](#spoken-answer-relay) can only record a provisional spoken answer. An unbound pane (or absent pane) counts as owner, just as for messages. This is **attribution, not authentication**: clients supply their pane, and processes running as the owner can claim an unbound one. Resolved/overruled log bodies retain the optional caller pane for report attribution; exact-repeat comparisons ignore that audit metadata. A missing historical caller is reported as not recorded.

The exact same answer (option, note and rule mark) again is an accepted decision no-op. A changed answer after a final resolution is refused, including adding a note or `--rule` afterwards. A different answer to a provisional choice is recorded as `overruled`. One atomic event reopens and unassigns its recorded, previously assigned, noncancelled dependent tasks, including accepted work and inherited child/review links. It clears the current submission/review/cost, retaining files, links, budgets and cumulative usage history. Unknown live task states refuse the overrule. No file, Git change, deployment or model turn is undone, and nothing is reassigned automatically. Read the new answer before deliberately assigning work again: stale submissions/verdicts fail while it is open, but after reassignment there is no attempt token to distinguish an old submission. `atm decide` prints the complete redo list and accepted-again progress; an exact repeat reports the existing redo without resetting work again. A stored deadline does not run an expiry timer. What happens to a decision while you are away is described under [Away](#away).

### Spoken-answer relay

From an unbound owner shell, show, designate or revoke the one durable relay slot. Use a registered full `project/name` address; a seat cannot designate itself:

```sh
atm relay
atm relay project/relay-seat
atm relay --clear
```

Setting another seat replaces the designation. It does not change the away switch, grant `decision.get`/list visibility or confirm/remove an earlier recorded answer; see the [relay visibility caveat](#known-limits-and-not-yet).

Only the designated **bound agent pane** can record your spoken listed option:

```sh
atm decide d41 yes --relay
```

A new relay requires a Pending, reversible decision: action/hire/budget and irreversible decisions still wait for your own answer. It can name any exact listed option, without a note or `--rule`, whether you are present or away. The record is `taken_provisionally`, with `by=owner` and a separate `relayed_by` seat: **not confirmed by you**. Repeating an already-provisional option is a no-op preserving its original taker/provenance; a relay cannot replace it with a different option.

From the unbound owner shell, choose one of these alternatives:

```sh
atm decide d41 yes  # confirm the recorded option
# Alternatively, choose a different listed option to overrule instead:
atm decide d41 no
```

Confirmation/overrule uses the existing final-answer and recorded-redo rules above, never a file rollback. A provisional intent-changing relay remains subject to the [stacking cap](#away) until you answer; returning or clearing the relay is not confirmation. After a timeout or uncertain result, inspect decisions and the inbox before any deliberate retry, as for other decision mutations.

### Answer from the TUI

Press **`a`** in Sessions or Work to inspect the selected named session's waiting decision. Multiple requests open a chooser; on a group, an unmapped row or a pane-sharing subagent, it opens the authorized global queue. This also reaches down, paneless and system requesters. A yellow **`?`** marks named-session leaves with waiting decisions in both layouts; **`n` / `N`** stop there as well as on agents needing input, opening collapsed ancestors and respecting the current filter. Marks use the daemon's seat-to-pane projection and fully paged decision list, not its capped attention summary. Reads and watch refreshes are not an atomic view of session activity; marks disappear while disconnected or watch is unavailable, and `a` explicitly refreshes the queue.

Inside the popup, no option is selected initially: **j/k** select an exact option, **↑/↓** scroll the full question/context and literal attachment references, and **Tab** edits an optional single-line note (8192 UTF-8 bytes). While editing, **Tab** and **Enter** are ignored (pasted tabs/newlines are dropped); only **Esc** finishes editing. Outside note editing, **Enter** records the explicitly chosen answer. Irreversible kinds and a different provisional answer require a default-No **`y`** confirmation naming the exact decision and answer. **Esc** backs out; while recording, input cannot submit another answer and Ctrl-C still quits. Text pasted while the note is not being edited runs as keys; bracketed paste is not enabled. Attachments are never read or opened. Standing rules remain CLI-only (`atm decide … --rule`). Control characters are escaped, but bidi overrides, zero-width characters and Unicode line separators are not escaped.

A [relayed provisional row](#spoken-answer-relay) is marked `relayed`. Inspect shows “Relayed by {seat} as your spoken answer; not confirmed by you”. Choose its recorded option and press Enter to confirm; another option uses the existing overrule confirmation.

Every request forwards the TUI's actual inherited caller pane, **not the selected agent's pane**. Only a genuinely unbound owner shell can answer; requester and manager panes may read their authorized requests but cannot resolve them. Success means **recorded**, not delivered or read by an agent. Overrules show the complete recorded redo IDs and progress, or an explicitly unavailable report; no files are reverted. Timeout, disconnect, refusal or a malformed mutation result retains the draft with an unknown-outcome warning: press **`i`** to inspect fresh state before any deliberate retry. Reconnection refreshes reads, never resubmits an answer. Final, withdrawn and expired decisions cannot be changed in the popup.

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
| `decision.get` | `id`; optional owner-only `redo_after=null`, `redo_limit=100` (1–1000) | `{"decision":<entry>}`, optional owner-only `redo` |
| `decision.resolve` | `id`, `option`; optional `note=null`, `standing_rule=false`, `relay=false` | `{"decision":<entry>,"changed":true}`, optional `redo` |
| `decision.withdraw` | `id`; optional `reason=null`; requester or owner only | Same mutation result |

For a [relay](#spoken-answer-relay), params contain only `id`, `option` and `relay:true`; `note` and `standing_rule` keys are forbidden even when null/false. The daemon derives the designated bound recorder, never a caller-supplied identity.

An entry reuses the decision's `id`, `request`, timestamps, `status`, provisional/resolution/withdrawal facts, and derived `class`. Status is authoritative; historical logs may retain an earlier fact under another status. Optional facts are null. `count_only=true` returns empty rows/null cursor and global owner counts to any caller; it excludes `after` and explicit `statuses`. Pages/get are bounded to 512 KiB without truncating facts; an oversized first row is refused. Continue with `next_after` while preserving your filters. There is no project filter for a system request with no structured project.

For overrules, `redo` contains `required_count`, `completed_count`, `cancelled_count`, creation-ordered `task_ids` and `next_after`. Continue its frozen ID set through `decision.get`; an ordinary `{id}` call still works on an older daemon and agents retain their existing decision visibility, without a cross-task redo view. The CLI drains these pages before writing output, using progress from the latest page. Nonoverrules omit `redo`; legacy unmeasured overrules have null counts, not zero. Complete results remain bounded to 512 KiB; a post-commit resolve/output error is not rollback.

Bounds are UTF-8 bytes: title 1–512; context ≤16384; 2–32 distinct nonempty options, each ≤1024; ≤32 nonempty file paths, each ≤4096; note/reason ≤8192. Recommendation must match an option exactly and a request deadline must be in the future. Unknown IDs return `not_found`, invalid params `invalid_params`, policy/identity refusals `refused`, unavailable persistence `unavailable`, and timeouts `timeout`. Accepted work can finish after timeout: inspect the list/get/inbox before retrying, rather than creating another request.

### Answers and persistence

The org event log retains requests and answers across daemon restarts. Recording an answer does **not** mean a harness accepted it: one daemon bridge durably enqueues a `system` / `answer` message to the requester, then the existing FIFO/guard/confirmation runtime delivers its one-line inbox reference. Read the body with `atm inbox --id m…`, then explicitly acknowledge it. Focused panes and occupied editors stay protected; no separate decision paste path bypasses those guards. Internal `system` requesters have no inbox and consume the durable resolution event instead.

The final decision is the bridge's recoverable work item; an exact canonical answer `msg.sent` is its completion marker, regardless of delivery/read status. Restart fills a missing enqueue and does not duplicate an existing one. An organisation snapshot or enqueue timeout backs off and reconciles from fresh committed state; the serial actor and exact-content check prevent duplicate late enqueues. A stopped actor or other terminal bridge failure requires restart; reads remain available when org persistence is healthy. Delivery failures/holds retain the normal message recovery rules. The canonical answer text is therefore a persistence correlation contract, not presentation-only text to change without considering old logs.

The same bridge also enqueues one rework note per invalidated task to its creator and former assignee (once when they coincide; system creators route to owner). Frozen recipients and exact `msg.sent` content make recovery independent of later reassignment, delivery or acknowledgement. Existing answer/away message texts remain unchanged.

New pending requests also run the owner's existing `[notify]` command, using a short `d41 from project/worker: atm decisions` reference, not private title/context/options. This shares the owner-message notifier described below; resolved, provisional, withdrawn or expired decisions suppress a delayed notice. External notices remain best effort, not the durable decision record.

## Away

```sh
atm away      # from a shell that is not an agent's pane
atm back
```

`atm away` tells the daemon you will not answer. Until `atm back`, every pending decision, whether asked before you left or after, is put through this table. While you are present the away rules take nothing automatically.

| The request says | While you are away |
|---|---|
| It can be undone and does not change the intent (a detail) | The agent that asked decides: its own recommended option is recorded, `by` the agent |
| It can be undone and changes the intent | Taken provisionally on the recommended option, `by` `system` |
| It cannot be undone or reaches outside | Waits for you |
| Its kind is `action`, `hire` or `budget`, whatever else it says | Waits for you |
| It names no recommended option | Waits for you |

The two fields that give a decision its class are filled in by the agent that asks, so the kinds that reach outside wait whatever they claim. Both recorded outcomes are the existing `taken_provisionally` state: they stay in `atm decisions`, and `atm decide` with the same option confirms one (`resolved`) and with another overrules it (`overruled`). `atm back` takes nothing back and answers nothing: it ends the absence and prints a complete ranked return digest, with recorded work an overrule would invalidate and what still waits.

The requester is told through the confirmed-message runtime, like an answer: one `system` message when its decision is taken provisionally, is its own to decide, or waits because you are away. `decision.request` itself answers as before. Your `[notify]` command still runs for a new pending decision.

**The stacking cap.** A provisional decision may carry the task in progress to its end; nothing else may be built on it until you have answered it. A task built on a decision names it in `rests_on` when it is created. Such a task may be created and stays `open`, but `task.assign` refuses it (`refused`) while the cap is reached, and lifts by itself once you answer the decision; being back is not enough. The cap reserves 1 for the unnamed task in progress and counts each reachable task ID ever assigned once: with the default of 1 no further ID starts; with N, at most N − 1 may. Reopening, cancellation and reassignment never refund a place; resuming the same ID spends no extra place. Follow recorded `rests_on`, `parent` and `judges` links across all projects, checking every inherited provisional intent-changing decision against its requester's project cap. A child's own decision does not replace an ancestor's. Details are uncapped, and already-assigned work is not stopped; older starts can already exceed a later cap. No sibling, ancestor or requester's unlinked original task is guessed.

[Relayed](#spoken-answer-relay) intent-changing provisional answers use this same recorded-dependency cap until you confirm or overrule them. Setting or revoking a relay does not set or clear your absence.

The cap is a setting per project, in `<project>/.atm/away.toml`:

```toml
stacking_cap = 1
```

No file means 1. The daemon reads the file of every registered project when you run `atm away`, records the caps with the switch, and prints them; a change takes effect at the next `atm away`. A value below 1, an unknown key or a file that is not TOML refuses `atm away` with the file's path, and you stay present. A decision's project is that of the agent that asked.

Only the owner sets the switch: as for `atm decide`, a pane bound to an agent is refused and an unbound pane counts as you. This is attribution, not authentication. The switch is in the org event log (`owner.away`, `owner.back`), so it survives a daemon restart; after one, the daemon handles whatever was left pending. Repeating away does not change the switch. Repeating back creates a fresh audited report for the last closed absence, updating work and numbers without another `owner.back`; before any absence it says so. Every report records `owner.digest_created`. Absence membership uses committed sequence boundaries, not wall-clock comparisons. Back and report creation are separate commits: retry back after an interrupted preparation; if another away intervenes, the error tells you to run back again. `--format json` prints one complete assembled result, preserving future fields. While you are away `atm status` ends with `away`, so a forgotten `atm back` shows in your status line; the mark comes with the decision count (`decision.list` adds `"away": true` to its result, and only then), so it is absent when that count cannot be read.

| Method | Params | Result |
|---|---|---|
| `owner.relay` | Empty shows; `agent` sets a registered full seat ID; `clear:true` revokes; agent and clear conflict | `agent` (seat or null), `changed` |
| `owner.away` | none | `changed`, `away_since`, `stacking_caps` (every registered project), `pending_count` (as the call found it) |
| `owner.back` | none | `changed`, `away_since` (null if you were not away), `provisional_count`, `pending_count`, optional `digest` |
| `owner.digest` | required `snapshot_seq`; optional exclusive row-index `after=null`, `limit=100` (1–1000) | Frozen `snapshot_seq`, `absence`, `relay` (seat or null), whole-report `counts` (including `relayed`), `total_rows`, ordered `rows`, `next_after` |

The digest ranks takes in the selected absence by noncancelled work ever started, then never-started work, then take sequence/ID. Accepted work is a subset of started, not extra weight. Earlier still-provisional decisions follow; still-waiting lists pending decisions and distinct cap-held/Waiting/Blocked tasks with their recorded reasons. Cancelled work is excluded explicitly. Recorded links only: unlinked work, including an unlinked task in progress, is not covered.

The header shows `Relay seat: <seat|none>`. Relayed spoken answers since the previous digest appear once, even if already confirmed; a relay already in the selected absence appears only under `taken`. Older unconfirmed relays remain under `earlier`. Each row names who relayed it. The `relayed` section and its count cover new relays outside `taken`, not lifetime relay totals; the relay seat is frozen with the digest.

Numbers cover **all recorded time and all projects**: distinct decisions taken for you (including requester-decided details), distinct owner overrules, and accepted-again tasks / the frozen required set per overrule. Reopening, starting, submitting, accepting a plan or recording cost is not completed rework; deliberate reassignment followed by an accepted result is. Cancellation stays incomplete without shrinking the denominator. Shared redo can count once under each affected overrule. Missing legacy metadata says `not recorded`, including a missing caller pane; an explicitly empty set is `0/0`.

Reports are immutable while paging, at most 100 rows and 512 KiB per default response. The CLI drains and validates every page before text/JSON output; no top-ten truncation or local ranking. Ordinary activity does not change that snapshot, and restart reconstructs it. A later back replaces it: stale pages return `conflict`, requiring back again. Oversized individual rows fail explicitly. `owner.digest` is owner-only and never creates a report. An old daemon missing `digest` retains count output with `Digest unavailable from this daemon; run atm decisions.`

**Rollback limitation:** an older daemon can replay the answer while ignoring new redo metadata and `owner.digest_created`, leaving formerly accepted/in-flight tasks valid instead of reopened. Do not use an older daemon to enforce invalidation on a new log. Conversely, upgrading never retroactively invalidates work for an old unannotated overrule; its redo is unmeasured. This is task-record invalidation, never a file rollback.

## Notifications

`atmd` can run a command of yours whenever a session starts needing input: a permission dialog, an MCP input dialog, or an agent asking you something, in any supported harness. The same command also handles newly committed Owner inbox messages, under the separate rules below. Add a `[notify]` section to `config.toml`. `atm setup` writes this example commented out, but it never rewrites an existing file, so add the section by hand if you already have one:

```toml
[notify]
command = ["notify-send", "atm", "--"]
```

`atmd` appends one message as the last argument, such as `example/main %5: claude needs input (Bash: cargo test)` (project and branch, pane, harness, and what it asks; a subagent's row says `claude subagent`), and runs the command directly. No shell parses the message. Keep the `--` for a command that takes options, so a message that starts with `-` isn't read as one. The message names the tool and a short summary of its input, such as the start of a shell command, so a script that sends it off your machine (Slack, ntfy) can trim it first.

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
command = ["/absolute/path/to/atm-slack"]
```

## How it works

```
Claude Code / pi / Codex  ──hook/extension──▶  atmd (daemon)  ◀──socket──  atm (TUI/CLI)
```

`atm setup` registers supported harness integrations (Claude Code hooks, the pi extension, and Codex CLI hooks). Harness events are forwarded to the `atmd` daemon over a Unix socket, and `atm` connects for real-time display. Codex context usage is read on a best-effort basis from the rollout transcript supplied with hook events; a missing or changed transcript never blocks lifecycle updates. Codex requires a one-time trust approval of the installed hooks: run `/hooks` inside codex after `atm setup`.

`atm-comms` is the library-only home of canonical durable-message rows, re-exporting existing core/org IDs and history-query types; it adds no second delivery service. `atm-chat` holds pure terminal-chat state and rendering, while the root `atm` program owns its separate connection and terminal I/O. Chat uses the existing daemon-backed message service; see [terminal chat](#terminal-chat) and the [pair/state upgrade limits](#upgrade).

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
agents or enable automatic handover. Named agents use `atm decisions request`
and `atm decisions withdraw` for durable owner choices (see [Decisions](#decisions)).
Named agents can inspect decisions only for themselves and verified reporting descendants, and get only delivery metadata for descendants' inboxes; a lead name grants no global visibility. The [return digest](#away) and [evaluation reports](#evaluation-report) are implemented. Drift scheduling is not shipped, and remaining unattended acceptance/model compliance is not proved by these commands. The skills expose these limits rather than silently bypassing them.

Source, per-rule reasons/model/harness verification records, installation and
usage: [`crates/atm/assets/atm-skills`](crates/atm/assets/atm-skills/README.md).
`atm uninstall` removes all four ATM skill folders, leaving unrelated skills.

## Documentation

See the **[Wiki](https://github.com/damelLP/agent-tmux-manager/wiki)** for the full user guide, tmux integration, architecture, and troubleshooting.

## Building from source and rolling out

The manifest requires Rust **1.88 or later**. From a Linux checkout:

```bash
git clone https://github.com/damelLP/agent-tmux-manager.git
cd agent-tmux-manager
cargo build --locked --release --bin atm --bin atmd
```

The resulting pair is `target/release/atm` and `target/release/atmd`. Building it does not update the installed daemon, hooks, extension or skills. Follow the upgrade procedure before replacing a live pair, then run the installed binary's `setup`.

### Source-checkout rollout

The repository includes Linux owner-run tools (Python 3.9+, Bash, git, tmux; source installs also need rustup/Cargo). They are not commands that agents should use to upgrade themselves.

**Try a reviewed commit separately first:**

```bash
scripts/atm-candidate install HEAD
scripts/atm-candidate installs
scripts/atm-candidate start
scripts/atm-candidate status
scripts/atm-candidate atm list -f json
scripts/atm-candidate attach       # run from a terminal OUTSIDE tmux
# Exit the attached client, then stop from outside the candidate server:
scripts/atm-candidate stop
```

The candidate kit builds an immutable SHA/profile-named binary pair, with a private socket, state and tmux server. It never runs `atm setup` and reuses your installed forwarders; `status` reports asset drift. It shares HOME, harness configuration and credentials, so it is **routing isolation, not a sandbox**. Its wrapper refuses `setup`, `uninstall` and `workspace`; do not bypass that wrapper with a direct candidate binary call. Stop before switching candidates; stop retains the per-install state. Full locations, cleanup and limits: [candidate kit](scripts/atm-candidate.md).

**Promote only when the owner has reviewed the candidate and protected-file changes.** Run from a checkout at the candidate commit, in a pane of the intended tmux server, with the environment intended for the restarted daemon. Choose absolute paths for the candidate install (the directory containing `install.json` and `bin/`), installed binary directory and daemon socket; on a first rollout also supply the commit of the installed build:

```bash
scripts/atm-rollout "$CANDIDATE_INSTALL" --bin-dir "$INSTALLED_BIN" --socket "$DAEMON_SOCKET" --installed-commit "$INSTALLED_COMMIT" --dry-run
scripts/atm-rollout "$CANDIDATE_INSTALL" --bin-dir "$INSTALLED_BIN" --socket "$DAEMON_SOCKET" --installed-commit "$INSTALLED_COMMIT"
```

Set those variables to your reviewed deployment's values; there is no implicit live destination. A dry run reads and reports; it is not the smoke test or an install. The real command asks you to accept the protected-file report, smokes the candidate on private endpoints with a model-free stand-in, checks for lifecycle/delivery work in flight, stops sidebars and daemon, promotes the pair and refreshes setup, restarts and checks it, and restores sidebars. A failed post-swap check attempts rollback. Leave the rollout terminal open; do not suspend it. Do not add `--real` unless you explicitly intend a real, potentially billed harness run; that route is not real-harness-verified.

Exit **0** means the selected pair passed the rollout checks, **1** a refusal (read whether it restarted the unchanged build), **2** usage, **3** a checked rollback, **4** `NEEDS YOU`. A `down` agent can still have a live pane and pass the checks: inspect warnings, not just the exit code. Rollback swaps binaries and managed setup assets, **not** organisation state or arbitrary configuration; compatibility with an older daemon remains a separate check. The pair replacement is not atomic across both files, and concurrent installers/autostarts are outside the tools' lock. Detailed recovery, the explicit `--rollback` command, protected-file reporting and limits: [promotion and rollout](scripts/atm-promotion.md).

#### Supervised probation

The owner-run source-checkout tool `scripts/atm-supervise` records stable/previous/candidate slots, checks an exact approved report and private rehearsal, then observes probation. Rehearsal records whether compatibility is **two-way** or **ONE-WAY**; it is not proof about the live organisation log. Only acceptance after probation rotates slots.

One automatic rollback is conditional on **unhealthy two-way probation**, both diagnostic logs retained, unchanged verified slots/pairs and rollout's own quiet checks. Unhealthy ONE-WAY probation, uncertain final health, refused recovery or missing evidence remains **NEEDS YOU**; there is no automatic retry, resume or slot reconciliation. Pair rollback still does not rewind organisation state, and candidate credential sharing remains routing isolation, not a sandbox. See the [supervisor procedure](scripts/atm-promotion.md#supervisor) and [known limits/manual recovery](scripts/atm-promotion.md#known-limits-and-the-manual-way-out); inspect retained evidence before intervention.

## Changelog

### 0.5.2

- **Work view:** compact footers show help first and only show extra hints that fit. Teammates stay with their reporting thread through unseated panes, mixed windows still show known work headlines, and captions omit empty fields.
- **Claude Code background shells:** ATM recognises the auto-mode footers in 2.1.327 and 2.1.328, so they no longer hold up delivery on otherwise ready screens.
- **Claude Code idle screens:** settled manual `/compact` and Stop-hook warning screens read as idle when the prior turn is visibly complete.
- **Screen, rollout and CLI fixes:** ATM reads pi screens with underlined links and Claude Code's `Restart to update` notice. Rollout allows bound seats pending revalidation. Help for `atm agent up` and `atm agent adopt` explains bare seat names.
- **Handover:** after saving, ATM can retry a clear refused because the editor wasn't ready and tells the seat's manager whether the handover completed.
- **Start and resume:** after a clear, the start line takes priority over ordinary queued messages. The new start line for a saved handover asks the replacement session to read its memory and resume open work, or report and stop.
- **Queued messages:** acknowledging a still-queued message with `atm msg ack` cancels its automatic delivery, including after a restart. It doesn't undo input already sent.
- **Unreadable screens:** if a seat's screen stays unreadable during delivery or revalidation, ATM sends one diagnostic note to its manager (or the owner if no manager is available) instead of silently waiting.
- **Reviewed fixes:** `atm task carry-back` applies the result of an accepted, independently reviewed fix to the original task without reassigning it.

### 0.5.1

- **Work-first tabs:** full and compact dashboards start on Work; `gt` / `gT` wrap through Work and Sessions, `1gt` / `2gt` select a tab, and `w` remains an alias. An older daemon initially falls back to Sessions once.
- **Work headlines:** reporting threads and task-first rows foreground the work, with repositories and agent names secondary. `gw` retains window grouping. Custom short work-title setting is not yet implemented; headlines fall back to known task titles, then names.
- **Chat window:** prefix C-t no longer adds a workspace sidebar to chat. For each existing workspace, run `atm workspace attach <name>` once (add `--isolate` for an isolated workspace). Close and reopen chat if it already has a sidebar.
- **Chat copy and logging:** copy long messages without hitting tmux's command-argument limit. Chat runs without a log if it can't open the log file.
- **Chat visibility:** chat uses fewer tmux calls to check whether its pane is visible.
- **Named agents:** `atm agent` subcommands have help text. `atm agent down` says the pane and its program keep running unless you pass `--kill`.

### 0.5.0

- **Work view:** press `w` to switch between Sessions and a window-grouped view of the work, retaining filtering, navigation and attention markers. Organisation overviews, task projects and evaluation reports make ownership, progress and recorded outcomes easier to inspect.
- **Decisions and relay:** inspect and answer waiting decisions from the TUI, with explicit option selection and confirmation for consequential changes. Designate a named relay seat to record your spoken answer provisionally; you still confirm or overrule it. Return digests and reports distinguish relayed answers, provisional choices and recorded rework.
- **Messaging and delivery:** scoped JSON message history/search, reporting-line delivery metadata and durable notice deduplication improve diagnosis. Giving up warns about a possible leftover reference without erasing the editor; inspect before retrying or editing. Pi reconnects can restore measured idle identity, but pending input/dialogs conservatively withhold that recovery. A receipt confirms acceptance of a reference, not task execution.
- **Supervisor and rollout:** source-checkout, owner-run supervision records stable/candidate slots and supports gated two-way rehearsal and probation. Unhealthy two-way probation can trigger one verified rollback if diagnostic capture and rollout's guards permit; uncertain or refused recovery needs the owner. Pair rollback does not rewind organisation state; candidate routing isolation is not a credential sandbox or an unattended-upgrade guarantee.
- **New crates:** `atm-comms` is a library-only, vendor-neutral home for the existing message row schema, re-exporting existing IDs/query types; it adds no second delivery service. Install the root `atm` package for the matched `atm`/`atmd` programs, not the library crate names.
- **Compatibility and known limits:** Rust 1.88+, protocol 1.1 with existing major-1 hook frames, pi 1.5.2+ and ATM-launched Codex 0.158.0+ remain the baselines. Upgrade the pair, rerun setup and reload integrations; do not assume older binaries can enforce newer log semantics. Pane attribution is not authentication; observed cost is not complete billing, and unattended skill/model compliance and drift scheduling remain unverified or unshipped.
- **Guarded delivery recovery:** an original attempt retained as having pasted the reference but provably sent no Enter may receive one freshly guarded Enter, without another paste or a new attempt. Uncertain input, focus, changed/ambiguous drafts, dialogs, running turns or changed identity still hold; there is no automatic owner retry or editor cleanup. A late receipt can still confirm the original delivery. This input-outcome proof does not survive daemon restart.
- **Owner chat:** library-only `atm-chat` and `atm chat` add a separate, window-first direct-message interface from an unbound owner pane, with prefix C-t switching, history and retained drafts. Sends use the existing confirmed-message service; uncertain sends are never automatically repeated. Seen marks require the last wrapped body line to be drawn with focused-client evidence and affect the shared Owner inbox; popup and desktop-focus limits still apply. Real desktop visibility and model compliance remain unverified; pane jumping, rooms, pickers and decision actions are not included.

### 0.4.2

- **Claude screen reading:** recognises the exact right-aligned `new task? /clear to save N tokens` hint and single-row dim suggestions under the drawn cursor, restoring message delivery, `atm agent clear` and adopt on those screens. Unrecognised layouts still hold.
- **Workspaces:** new sidebars span existing window splits; sidebar resizes act on the window that changed, not whichever window a client is viewing; same-name workspaces on different tmux socket labels no longer share hook scripts. Refresh existing hooks with `atm workspace attach <name>` (isolated: `atm workspace attach <name> --isolate`); 0.4.1 hooks still work until then.
- **Installer:** when the install directory is not on PATH, the suggested `export PATH=...` line names that directory.

### 0.4.1

- Includes the pi final-idle forwarder and manual compaction outcomes. Use **pi 1.5.2 or later** (1.5.2 tested); `agent_settled` first shipped in **pi 0.80.4**. The forwarder now sends `agent_settled`, not the low-level `agent_end` that can precede continuation or recovery.
- Follow [Upgrade](#upgrade): stop the old daemon and wait for exit, replace `atm` and `atmd` together, run the new `atm setup`, start the new daemon, then start new pi sessions. An old running daemon ignores the new settlement events.
- Extension-consumed idle input can still leave a pi row Working; the input idle correction remains deferred to avoid double-counted turns and automatic-handover loops.

### 0.4.0

Adds daemon-backed, named-agent coordination alongside the existing tmux dashboard and pane-control CLI. This release ships the landed commands; remaining coordination work follows in 0.5.0. The wire protocol remains **1.1**, and the source MSRV is **Rust 1.88**.

#### New

- **Project-local named agents.** Roles in `.atm/roles/<name>.md`, project registration in `.atm/project.toml`, and stable `project/name` identities. `atm agent up`, `adopt`, `clear`, `respawn`, `down` and `list` manage lifecycle; bare targets resolve within the caller's project.
- **Session recovery and handover.** `atm me` finds the current role, existing memory, unread work and reporting command. A successful start/clear queues a guarded start-line message. `atm agent handover` requires a changed memory file and an ended save turn before clearing. Automatic context/idle handover is opt-in, off by default.
- **Durable messages and inboxes.** Queuing, harness-confirmed delivery and explicit read acknowledgement are separate. Attachment paths are literal references, not uploads. Held deliveries have explicit inspection/retry/give-up controls.
- **Tasks and reviews.** Create, assign, submit, review and record verdicts with `atm task`. Cancellation, reassignment, withdrawn submissions and displaced live reviews can notify the relevant holders/creator through guarded inbox references. These notices do not interrupt a running turn or automatically cancel a review: inspect current state and assignee before continuing.
- **Observed usage, budgets and reminders.** Prompt turns, task-state time, review/rework counts, cumulative-cost deltas and peak reported context. Optional cost/turn/deadline budgets notify rather than stop work; idle reminders surface tasks left in an acting state. Missing measurements stay unknown, not zero.
- **Owner choices and policy.** Named agents request/withdraw durable decisions; owners inspect and answer with `atm decisions`/`atm decide`. `--rule` can append a standing rule to the requester's project. Owner-approved `.atm/intent.md` restricts listed task measures at creation without executing a judge.
- **Away/back policy.** Explicit reversible/provisional/wait handling of pending decisions, with a per-project stacking cap for tasks explicitly resting on a provisional intent-changing decision. Returning does not undo earlier choices; irreversible/external actions still wait for the owner.
- **Persistent organisation state.** Event-log replay after daemon restart, with `ATM_STATE_DIR` for persistent PID/log/event/TUI state. This cannot recover history an older release only kept in memory.
- **Source trial and rollout tools.** Private-endpoint candidate trials and owner-run protected-file review, model-free smoke, pair promotion, restart checks and attempted rollback. Explicit paths are required; this is not an unattended updater or a credential sandbox.

#### Integration and control

- Setup refreshes four ATM skills per detected harness: draft `atm-lead`/`atm-member` for named agents, plus legacy `atm-coordinate`/`atm-implement` for unnamed panes. Do not mix their protocols; role metadata does not load a skill.
- Claude setup preserves an existing custom `statusLine`; forward its stdin to `atm-hook` for ATM context/model/cost. Uninstall keeps custom commands and removes/restores only an exact ATM-generated entry.
- Codex launches add required `--no-daemon` arguments and require codex-cli **0.158.0 or later**. Remove redundant raw overrides, make wrappers forward required arguments, and approve hooks with `/hooks` after setup.
- pi context/turn-end/agent-end frames omit unneeded conversation bodies while retaining envelope identity/model/usage. This is not a universal frame-size cap, heartbeat or final-idle fix; 0.4.0 retains the existing forwarder.
- `atm agent clear` now works with pi's **Update Available** notice on built-in themes. With a custom theme whose notice ATM cannot recognise (neither faint labels nor a bold title), clear still refuses safely.
- Message admission refuses an unconfirmed pending process row for a known real-session agent. A successful `queued` receipt still is not delivery confirmation; inspect identity and inbox rather than blindly resending.
- Immediate input and daemon control share reservation checks. `atm send` refuses synchronized panes, disabled input, unsafe controls, copy mode and dialogs, but can merge an existing draft. `atm reply -y/-n` can wait without rechecking the released screen.
- Coordinator pane tags survive harness restart/clear and appear in JSON session listings and the TUI. Dashboard, workspaces, layouts, popup picker, notifications and immediate controls remain available.
- `atmd stop` waits for process exit, reports progress after five seconds and gives up after 75 seconds without force-killing or deleting state. Blocking I/O can outlast that allowance; a timeout does not permit swapping a still-running pair.

#### Install, upgrade and limits

On Linux, install the **`atm` crate** from crates.io for both `atm` and `atmd`; `atmd` and `atm-tui` are library-only packages. Use Rust **1.88 or later**:

```sh
cargo install --locked atm
atm setup
```

Initialize/configure the chosen harness in this HOME, including any required provider/login setup, and complete [setup verification](#verify-harness-setup) before the [named-agent quick start](#named-agentproject-quick-start). An executable on PATH, setup exit 0 or installer completion alone is insufficient. For pi, check the installed package files and `packages/pi-atm` registration before starting/restarting.

For an upgrade, save context, quit every TUI/sidebar and caller that could autostart the daemon, stop the old daemon and wait for confirmed exit, preserve state/config, replace both programs through one install route, rerun/verify setup, then restart in the intended environment and inspect sessions, agents and inboxes. See [Upgrade](#upgrade) and the [source rollout procedure](#source-checkout-rollout).

Major-1 hook frames remain compatible with protocol 1.1, but new RPC methods need the matching daemon. Rollback does not undo organisation state or arbitrary configuration; an interrupted non-atomic pair swap needs explicit recovery before restart. Older-state compatibility remains a separate check.

**Known pi limit:** `atm agent clear` works with pi's **Update Available** notice on built-in themes. With a custom theme whose notice ATM cannot recognise (neither faint labels nor a bold title), clear still refuses safely. The final-idle forwarder update is not included in 0.4.0. Named-skill compliance, review digest, drift scheduler and remaining end-to-end coordination acceptance are not completed by this release; see [known limits and not yet](#known-limits-and-not-yet).

A receipt confirms acceptance of a reference, not model reading/compliance or task completion. Local pane attribution is not authentication against hostile same-user clients. Task usage is observed accounting, not billing; budgets only notify. External notifications are best effort. Rollout checks can pass with a down agent's pane still alive: read warnings, not just the exit code. Real-harness rollout smoke remains unverified; `--real` can start billed turns.

## License

MIT — see [LICENSE](LICENSE).
