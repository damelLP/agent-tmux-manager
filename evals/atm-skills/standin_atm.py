"""Stand-in `atm` for the atm skills' decision tests.

It serves a recorded situation and logs every call. It never talks to atmd
or tmux. The run directory (--dir) holds:

  list.json       what `atm list -f json` prints: a row for every pane
  peek-<N>.txt    the screen `atm peek %N` prints
  scenario.json   optional: {"watch": "%7", "stop_after_writes": 1}

After `stop_after_writes` writes (send, reply, interrupt, kill, spawn) to the
watched pane, its session is gone: `list` leaves its row out and `peek` fails
as atm does for a pane that no longer exists. That ends a run soon after the
decision under test. Writes and refusals use atm 0.2.7's own wording
(src/bin/atm/cli.rs).
"""

import json
import os
import sys
import time

SEND_HELP = """Send text to an agent's tmux pane

Pastes the text, then presses Enter, as if the user typed it: `/clear` runs, and a half-typed draft in the input merges with the text. Refuses a session waiting on a dialog (answer it with `atm reply`), a subagent that shares its lead's pane, a pane in copy mode, and multi-line text for a program that didn't ask for bracketed paste (tmux 3.7 and later report this; with older tmux, multi-line text is sent).

Usage: atm send [OPTIONS] <TARGET> <TEXT>

Arguments:
  <TARGET>
          Session ID (short form) or tmux pane ID

  <TEXT>
          Text to send

Options:
  -p, --pick
          Pick mode: exit after jumping to a session (fzf-style picker)

  -h, --help
          Print help (see a summary with '-h')
"""

WRITES = {"send", "reply", "interrupt", "kill", "spawn"}


def main(argv):
    run_dir, log_path, args = argv[1], argv[2], argv[3:]
    scenario = load_json(os.path.join(run_dir, "scenario.json"), {})
    watch = scenario.get("watch")
    gone_flag = os.path.join(run_dir, "gone")
    gone = os.path.exists(gone_flag)
    rows = [
        r
        for r in load_json(os.path.join(run_dir, "list.json"), [])
        if not (gone and r.get("tmux_pane") == watch)
    ]

    cmd = args[0] if args else ""
    pane = resolve(rows, args[1]) if len(args) > 1 and cmd in WRITES | {"peek"} else None
    entry = {"t": round(time.time(), 3), "argv": args, "pane": pane}
    with open(log_path, "a") as f:
        f.write(json.dumps(entry) + "\n")

    if cmd in ("--version", "-V"):
        print("atm 0.2.7")
        return 0
    if "--help" in args or "-h" in args:
        if cmd == "send":
            print(SEND_HELP, end="")
        else:
            print(f"Usage: atm {cmd} [OPTIONS]  (stand-in: run `atm {cmd} --help` on a real atm for the flags)")
        return 0
    if cmd == "list":
        return do_list(rows, args[1:])
    if cmd == "peek":
        return do_peek(run_dir, pane, args)
    if cmd in WRITES:
        if pane is None and cmd != "spawn":
            target = args[1] if len(args) > 1 else ""
            print(f"Error: No session found matching '{target}'", file=sys.stderr)
            return 1
        rc = do_write(rows, cmd, pane, args)
        if rc == 0 and pane is not None and pane == watch:
            count_write(run_dir, scenario, gone_flag)
        return rc
    print(f"atm stand-in: `atm {cmd}` isn't part of this test", file=sys.stderr)
    return 2


def load_json(path, default):
    try:
        with open(path) as f:
            return json.load(f)
    except FileNotFoundError:
        return default


def resolve(rows, target):
    """A pane id passes through, as atm does; a session id prefix maps to its pane."""
    if target.startswith("%"):
        return target
    hits = [r for r in rows if r.get("id", "").startswith(target)]
    return hits[0].get("tmux_pane") if len(hits) == 1 else None


def do_list(rows, opts):
    fmt, status, project = "table", None, None
    i = 0
    while i < len(opts):
        o = opts[i]
        val = opts[i + 1] if i + 1 < len(opts) else ""
        if o in ("-f", "--format"):
            fmt, i = val, i + 2
        elif o == "--status":
            status, i = val, i + 2
        elif o == "--project":
            project, i = val, i + 2
        elif o.startswith("--format="):
            fmt, i = o.split("=", 1)[1], i + 1
        else:
            i += 1
    if status:
        want = "attention_needed" if status == "attention" else status
        rows = [r for r in rows if r.get("status") == want]
    if project:
        rows = [r for r in rows if project in (r.get("project_root") or "")]
    if fmt == "json":
        print(json.dumps(rows, indent=2))
    elif fmt == "ids":
        for r in rows:
            print(r.get("id_short", ""))
    else:
        print(f"{'ID':<10} {'PANE':<6} {'HARNESS':<8} {'STATUS':<17} {'CTX':>6}  DIR")
        for r in rows:
            ctx = f"{r.get('context_percentage', 0):.0f}%"
            print(
                f"{r.get('id_short', ''):<10} {r.get('tmux_pane') or '-':<6} {r.get('harness', ''):<8} "
                f"{r.get('status', ''):<17} {ctx:>6}  {r.get('working_directory', '')}"
            )
    return 0


def do_peek(run_dir, pane, args):
    path = os.path.join(run_dir, f"peek-{(pane or '').lstrip('%')}.txt")
    gone = os.path.exists(os.path.join(run_dir, "gone")) and pane_is_watched(run_dir, pane)
    if pane is None or gone or not os.path.exists(path):
        shown = pane or (args[1] if len(args) > 1 else "")
        print(
            f"Error: Failed to capture pane {shown}\n\nCaused by:\n"
            f"    tmux command failed: capture-pane — can't find pane: {shown}",
            file=sys.stderr,
        )
        return 1
    with open(path) as f:
        lines = f.read().splitlines()
    tail = None
    for i, a in enumerate(args):
        if a in ("-n", "--tail") and i + 1 < len(args):
            tail = int(args[i + 1]) if args[i + 1].isdigit() else None
        elif a.startswith("--tail="):
            tail = int(a.split("=", 1)[1])
    if "--prompt" in args:
        # Best-effort like atm's: with no dialog it may print a single line.
        nonblank = [l for l in lines if l.strip()]
        print(nonblank[-1] if nonblank else "")
        return 0
    print("\n".join(lines[-tail:] if tail else lines))
    return 0


def pane_is_watched(run_dir, pane):
    return load_json(os.path.join(run_dir, "scenario.json"), {}).get("watch") == pane


def do_write(rows, cmd, pane, args):
    row = next((r for r in rows if r.get("tmux_pane") == pane), None)
    if cmd == "send":
        if row and row.get("status") == "attention_needed":
            print(
                f"Error: Refusing to send to {pane}: the session is waiting on a dialog that Enter "
                "would answer. Answer it with `atm reply`.",
                file=sys.stderr,
            )
            return 1
        print(f"Sent to {pane} by tmux paste")
    elif cmd == "reply":
        print(f"Sent Escape to {pane}" if "-n" in args or "--no" in args else f"Sent Enter to {pane}")
    elif cmd == "interrupt":
        print(f"Interrupted {pane}")
    elif cmd == "kill":
        print(f"Killed {pane}")
    # spawn prints nothing, as atm 0.2.7 does.
    return 0


def count_write(run_dir, scenario, gone_flag):
    counter = os.path.join(run_dir, "writes")
    n = int(open(counter).read()) + 1 if os.path.exists(counter) else 1
    with open(counter, "w") as f:
        f.write(str(n))
    if n >= scenario.get("stop_after_writes", 1):
        open(gone_flag, "w").close()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
