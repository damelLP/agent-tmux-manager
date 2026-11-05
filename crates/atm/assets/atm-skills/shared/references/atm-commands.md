# atm command reference for coordinators

The commands a coordinator uses to drive implementers, with what each one
refuses and what to do instead. It follows atm's code after #119 and #145
(`src/bin/atm/cli.rs`, `src/bin/atm/spawn.rs`). Run `atm <command> --help`
for the full flags.

## Targets: use the pane

Every command below takes a session id prefix or a tmux pane id (`%12`).
**Use the pane id.** A pane stays the same for an implementer's whole life,
but its session id doesn't:

- Claude Code gives the same process a new session id on `/clear`.
- A short id prefix can match two sessions (`Ambiguous target`).

Take the pane from the `tmux_pane` field of `atm list -f json`. Pane ids are
per tmux server: atm acts on the server it talks to (the default one, or the
`tmux -L` label in `ATM_TMUX_SOCKET`), and a session in another server (`atm
workspace create --isolate`) can show the same `%N` in `atm list`.

## `atm list -f json`

Prints a JSON array, one object per session atmd knows. Fields a coordinator
reads:

| Field | Meaning |
|---|---|
| `tmux_pane` | `"%12"`, or `null` outside tmux |
| `harness` | `"claude"`, `"pi"`, `"codex"`, ... |
| `status` | `"idle"`, `"working"` or `"attention_needed"` (a dialog waits for an answer) |
| `parent_session_id` | set on a subagent. An in-process subagent (Claude's background agents) has its lead's pane |
| `id`, `id_short` | the session id. A clear that starts a new session gives the pane a new id: Claude's `/clear`, pi's `/new` |
| `last_activity` | an RFC 3339 time. Metadata events move it while the session is idle, so it doesn't prove a turn ran |
| `project_root`, `worktree_path` | the git project (a worktree resolves to its main checkout) and the worktree. Record these in notes and handovers |
| `context_percentage` | how full the session's context is, as its harness reports it (Claude's status line, pi-atm, Codex's transcript). Your own row's is your context. It reads `0` when atm doesn't know: no report yet, after `/clear`, or pi after a compaction until its next reply. Under Codex's default shared app-server daemon every Codex session reports the daemon's pid and pane, so a Codex session's row may show another pane or be folded into another session's (atm-codex-adapter docs); `codex --no-daemon` avoids it |
| `worktree_branch`, `first_prompt` | for your notes and handover |
| `working_directory` | for display only: cut to `...` plus the last 27 characters when longer than 30 |

Filters: `--status working|idle|attention`, `--project <part of the project
root>`. A session appears after its harness's first event to atmd. `atm list`
starts atmd if none is running.

## `atm peek <pane> [-n N] [--prompt]`

Prints the visible screen of the pane (not its scrollback). `-n 60` keeps the
last 60 lines, but a small pane shows fewer. An implementer's report ends with
a line starting `QUESTION:`, `BLOCKED:`, `WAITING:` (a background job of its
own still runs; it reports in its next turn) or `DONE:`.

`--prompt` tries to cut out a Claude Code dialog (the question and its
numbered options). It's best-effort: with no dialog it may print a single
line, and a report that ends in a numbered list can come back as the
"prompt". Confirm with a plain `atm peek <pane> -n 40`.

Read before every write: a peek shows a dialog, a typed draft or a busy
screen that a send would collide with.

## `atm send <pane> <text>`

Pastes the text into the pane, then presses Enter, as if the user typed it.
`/clear` runs as a command, and `!cmd` runs a shell command.

Quote the text with single quotes (`atm send %N '...'`, an apostrophe as
`'\''`). In double quotes your own shell expands `$atm-implement` to
`-implement` and runs anything in backticks before atm sees it.

It refuses, with a reason:

| Refusal | Do instead |
|---|---|
| "the session is waiting on a dialog that Enter would answer" | `atm peek <pane> --prompt`, then `atm reply`. The status can outlast a dialog dismissed with Esc: if the screen shows no dialog, ask the user |
| "the session is a subagent in its lead's pane" | Target the lead's pane (`%N`), and only if you mean to talk to the lead |
| "the pane is in copy mode" | Ask the user to leave copy mode (`q`) in that pane |
| "the program didn't ask for bracketed paste, so each line would submit alone" | Send one line. Claude, pi and Codex all ask for it, so this means something else runs in the pane |

What it doesn't protect against:

- **A draft in the input box merges with the text.** The user may type into
  an implementer's pane, for example to answer it directly; a draft left
  there merges with your next send, and atm can't see it.
- **A busy session.** Claude queues the text until its turn ends, pi queues it
  as steering, and Codex steers it into the running turn at once (#116). pi's
  `/new` while busy aborts the turn. Clear an implementer only when it's idle.
- **A send right after `/clear`.** Two sends a few milliseconds apart reached
  Claude Code 2.1.316 as one plain-text message (`/clear/atm-implement ...`)
  and nothing was cleared. Wait until `atm list` shows a new `id` on the
  pane, then send. Checked on Claude Code 2.1.317 with atm and atmd 0.2.7, in
  a sealed rig with a fake model API: the new id appeared 52 to 103 ms after
  `/clear`, and a send the moment it appeared ran as a slash command in a
  cleared context in 10 of 10 trials; a send 6 ms after `/clear` merged with
  it. pi 0.73.1's `/new` clears the screen ("✓ New session started") and gives
  the pane a new row (atm-pi-adapter docs). Codex 0.163.0's `/new` is
  unchecked; if no new id appears, peek for a fresh screen (the previous
  output gone, no turn running). Claude's input line may show the command you
  just ran in grey (`❯ /clear`): that's a suggestion, but a plain peek can't
  tell it from typed text.
- **Claude Code's paste limit.** Claude collapses a paste of more than 800
  characters or more than 3 lines into `[Pasted text #N]` and then sends it to
  the model as plain text: a slash command in it doesn't run, so the skill
  doesn't load. Keep a skill call to one short line and put the brief in a
  file. pi and Codex have no such limit.

## `atm reply <pane> [-y | -n | -o N]`

Answers a dialog by pressing keys: `-y` presses Enter on the highlighted
option, `-n` presses Escape, `-o N` moves Claude's `❯` cursor to option N and
presses Enter. With no flag it presses Enter.

It refuses nothing: the keys go to whatever is on screen. Peek with
`--prompt` first, and reply only when you know what you're approving.
Otherwise ask the user.

Claude Code sends no hook when a dialog is answered. After `-y` the row reads
`attention_needed` until the approved tool ends, so wait with
`atm-wait --after-reply`. After `-n` it reads that way
until the next prompt, and `atm send` refuses the session, so the next
instruction has to be typed in the pane (#127 plans `atm reply --no
--message`).

## `atm interrupt <pane>`

Presses Ctrl+C in the pane, which stops the current turn. Never interrupt an
idle implementer, and never twice in a row: at an empty prompt, Claude Code
asks for a second Ctrl+C and then exits. On a subagent's row it hits the
lead's pane (#146).

Claude Code sends no hook for an interrupt, so its row keeps reading
`working` until the next prompt, and `atm-wait` never sees the turn end. Peek
instead: an idle input box means it stopped.

## `atm kill <pane>`

Kills the pane and the harness in it. Work on disk stays. On a subagent's row
it kills the lead's pane (#146). Kill an implementer only when the user asks
or the loop is over.

## `atm spawn -H <harness> -w <dir>`

Splits your own pane (`$TMUX_PANE`) and starts the harness in the new pane,
in `<dir>`. Options: `-d left|right|above|below` (default below), `-s 30%`
(default 50%), `-m <model>`, and `-t <pane>` to split another pane.

- **It prints the new pane's id** (`%13`) as soon as the pane exists:
  `pane=$(atm spawn -H pi -w <dir>)`. `-f json` prints
  `{"pane": "%13", "harness": "pi", "cwd": "<dir>"}`. A failure after the
  id is printed means the pane exists but the harness may not have
  started: `atm peek` it, and `atm kill` it if you don't need it.
- **The pane has no row in `atm list` until its harness reports.** Claude
  Code and pi report at startup. Codex 0.163.0 reports at its first prompt,
  so its row appears only after your first send. `atm send %N` works
  without a row.
- **Wait for the harness before the first `atm send`.** Text sent before it
  has started (up to about 0.4 s after spawn on a warm machine, longer on a
  cold one) lands in the input box but isn't submitted: the terminal isn't
  in raw mode yet, so Enter arrives as a newline (Claude Code 2.1.318, pi
  0.73.1, Codex 0.163.0). Wait until `atm peek %N` shows the input box, or
  for the row (Claude Code, pi).
- **A Codex coordinator** under Codex's default shared app-server daemon
  sees the `$TMUX_PANE` of the Codex TUI that started the daemon, not its
  own (0.163.0). atm then splits that pane, or, if it's gone, fails with
  "Failed to split $TMUX_PANE (%N); pass -t <pane> ...". Run the
  coordinator as `codex --no-daemon`, or ask the user for your pane and
  pass `-t`.
- Spawn only when the user asks or agrees.

## A Codex coordinator needs its sandbox opened

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

## Not in atm yet

| Planned | Today |
|---|---|
| `atm name` and targets by name (#118) | keep a name-to-pane table in your notes and handover |
| `atm role` and the TUI's `C`/`I` tag (#148) | nothing; skip the step |
| `atm profile show <name>` (#147) | `scripts/atm-profile <name>` |
| `atm spawn --prompt` for the first brief (#121) | spawn, wait for the input box, then `atm send` |

## Scripts in this skill

They are the steps the skills run, not atm commands. `atm setup` installs
them in each skill's `scripts/` folder. Paths are relative to this file. Run
them by absolute path from your own working directory.

- **`../scripts/atm-wait [--after-reply] <pane> [timeout] [start-timeout]`**
  waits for an implementer's turn to end by polling `atm list -f json`. atmd
  already tracks idle, so there is no `atm wait` (#122 was closed). Run it in
  the same command as the send that starts the turn
  (`atm send %N '...' && bash <path>/atm-wait %N`), so a short turn can't end
  between the two. A dialog
  at its first poll returns `ATTENTION`; `--after-reply` counts it as
  answered, for waiting right after you approved one.
- **`../scripts/atm-profile <name>`** prints a profile (#147's lookup).
