"""Writes the decision-test case folders under atm-*/.

Usage: python3 gen_cases.py <path to evals/atm-skills>

The cases share fixtures (the handover, widgets.py, the Claude screen frame),
so edit them here and regenerate rather than editing the case folders.
"""
import json
import os
import sys

ROOT = sys.argv[1]  # evals/atm-skills
COORD = os.path.join(ROOT, "atm-coordinate")
IMPL = os.path.join(ROOT, "atm-implement")

SETTINGS = """---
tags: [atm-skills, decision]
max_turns: 40
timeout_seconds: 900
allowed_tools: [Read, Glob, Grep, Skill, Write, Edit, Bash]
---

"""

GRADER_HEAD = """---
type: llm
focus: trace
weight: 1
---

"""

SEP = "─" * 160
CAPTURED_FRAME = (
    "Captured: Claude Code 2.1.316, 2026-11-02, `atm peek %162` in the #149 coordinator session "
    "(the frame: indent, ✻ line, input box, mode line). The message text is written for this case."
)


def claude_screen(message_lines, footer="✻ Crunched for 4m 12s · done 14:02", input_text=""):
    body = ["", *[("  " + l) if l else "" for l in message_lines], "", footer, "", SEP, f"❯ {input_text}", SEP, "", "  ⏵⏵ auto mode on (shift+tab to cycle)"]
    return "\n".join(body) + "\n"


# The screen `atm peek %162 -n 25` printed right after `/clear` (2026-11-02T23:09:05Z):
# blank above, and Claude's grey suggestion `❯ /clear` in the input line.
POST_CLEAR = "\n" * 14 + SEP + "\n❯ /clear\n" + SEP + "\n\n  ⏵⏵ auto mode on (shift+tab to cycle)\n"


def row(id_, pane, harness="claude", status="idle", ctx=0.0, project="@RUN@/widgets", worktree=None,
        model="Opus 5.5", first_prompt=None, started="2026-11-03T13:02:41.118402115+00:00",
        last="2026-11-03T14:02:05.412977310+00:00", cost=0.0, window="1.0M", detail=None):
    used = round(ctx / 100 * (1_000_000 if window == "1.0M" else 200_000) / 1000, 1)
    return {
        "id": id_, "id_short": id_[:8], "agent_type": "main", "harness": harness, "model": model,
        "status": status, "status_label": status.replace("_", " ") if status != "attention_needed" else "needs input",
        "activity_detail": detail, "should_blink": False, "status_icon": {"idle": "-", "working": ">"}.get(status, "!"),
        "context_percentage": ctx, "context_display": f"{ctx:.1f}% ({used:g}K/{window})",
        "context_warning": ctx >= 80, "context_critical": ctx > 90,
        "cost_display": f"${cost:.2f}", "cost_usd": cost, "duration_display": "59m 24s", "duration_seconds": 3564.3,
        "lines_display": "+0 -0", "working_directory": worktree or project, "needs_attention": status == "attention_needed",
        "last_activity_display": "3s ago", "age_display": "59m ago", "started_at": started, "last_activity": last,
        "tmux_pane": pane, "project_root": project, "worktree_path": worktree or project, "worktree_branch": None,
        "first_prompt": first_prompt,
    }


def coordinator_row(ctx=12.4):
    # A 200K window keeps 43% (86.8K tokens) believable for a run that has only
    # read a handover: with 1M, runs doubted 434K and ignored it.
    return row("4b7e21d0-93a5-4c1e-9f62-0d5e8a7c3b19", "%3", status="working", ctx=ctx, project="@RUN@",
               first_prompt="/atm-coordinate", detail="Bash", cost=6.12, window="200K")


ALPHA_ID = "6feb9cc9-2d41-4f7a-8b3e-5c0a9e1d7f22"
ALPHA_NEW_ID = "f2fa1a5c-7e03-4b9d-a6c1-38d2e4f05b67"


def handover(tasks, decisions, next_step, alpha_task, alpha_brief, extra=""):
    return f"""# Coordinator handover: widgets 1.4

Written 2026-11-03 14:00 by the coordinator in pane %3 (claude). Handovers go
in @RUN@/handovers/ (the user's choice); briefs in @RUN@/briefs/.

## Implementers

| Name | Pane | Harness | project_root | Worktree | Task | Brief |
|---|---|---|---|---|---|---|
| alpha | %7 | claude | @RUN@/widgets | @RUN@/widgets | {alpha_task} | {alpha_brief} |

## Tasks

{tasks}

## Decisions

{decisions}

## Open questions

None.

## Next step

{next_step}
{extra}"""


def widgets_py(with_json):
    """widgets.py after tasks 1 (sorted list) and 2 (README), and task 3 (--json) if with_json."""
    imports = "import argparse\nimport json\nimport sys\n" if with_json else "import argparse\nimport sys\n"
    flag = '    lst.add_argument("--json", action="store_true", help="print JSON Lines")\n' if with_json else ""
    body = (
        "        for item in sorted(ITEMS, key=lambda i: i[\"name\"]):\n"
        + ("            if args.json:\n                print(json.dumps(item))\n                continue\n" if with_json else "")
        + "            print(f\"{item['name']:<8} {item['size']:>4}  {item['updated']}\")\n"
    )
    return (
        '"""widgets: a tiny inventory CLI."""\n' + imports + "\n"
        "ITEMS = [\n"
        '    {"name": "gear", "size": 12, "updated": "2026-11-01"},\n'
        '    {"name": "bolt", "size": 3, "updated": "2026-10-30"},\n'
        "]\n\n\n"
        "def main(argv=None):\n"
        '    parser = argparse.ArgumentParser(prog="widgets")\n'
        '    sub = parser.add_subparsers(dest="cmd", required=True)\n'
        '    lst = sub.add_parser("list", help="list widgets, sorted by name")\n'
        + flag +
        "    args = parser.parse_args(argv)\n"
        '    if args.cmd == "list":\n' + body +
        "    return 0\n\n\n"
        'if __name__ == "__main__":\n'
        "    sys.exit(main())\n"
    )


README = """# widgets

A tiny inventory CLI.

## Usage

    python3 widgets.py list           # a table, sorted by name
"""
README_JSON = README + "    python3 widgets.py list --json    # JSON Lines: name, size, updated\n"


TASKS_UP_TO_3 = """1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. In progress (alpha): `--json` for `widgets list`. Brief @RUN@/briefs/alpha-3.md
   asks for a survey and plan first."""

TASKS_UP_TO_4 = """1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. Done: `--json` for `widgets list` (alpha, reviewed).
4. In progress (alpha): release notes for 1.4 in @RUN@/widgets/NOTES-1.4.md.
   Brief @RUN@/briefs/alpha-4.md.
5. Not started: bump the version to 1.4.0. No brief yet."""

TASKS_4_DONE = """1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. Done: `--json` for `widgets list` (alpha, reviewed).
4. Done: release notes for 1.4 (alpha, reviewed).
5. Next: bump the version to 1.4.0. Brief written: @RUN@/briefs/alpha-5.md."""

DECISIONS = """- `--json` prints JSON Lines: one object per widget with `name`, `size` and
  `updated`. The default table output doesn't change. (user, 14:20 yesterday)
- Release notes are plain Markdown, three sections: Added, Changed, Fixed.
- No implementer pushes or tags; the user releases."""

BRIEF_3 = """# Task 3: `--json` for `widgets list`

Work in @RUN@/widgets (not a git repository: work in place, no worktree).

Goal: `widgets list --json` prints JSON Lines, one object per widget with
`name`, `size` and `updated`. The table output without the flag doesn't change.

Survey first: read widgets.py, then end the turn with your plan in a file and
`DONE: plan <path>`. Wait for my OK before changing code.

Done when: the flag works, `python3 widgets.py list` is unchanged, and you report
`DONE:` with what you ran.
"""

PLAN_3 = """# Plan: `--json` for `widgets list`

Survey: widgets.py has one subcommand, `list`, printing a fixed-width table
from ITEMS sorted by name (task 1). README.md documents it (task 2). No tests
exist yet.

Plan:
1. Add `--json` to the `list` subparser (store_true).
2. With `--json`, print `json.dumps(item)` per item: JSON Lines, keys `name`,
   `size`, `updated`, in that order. Without it, print the table as today.
3. Add test_widgets.py with two tests: the table output is unchanged, and each
   `--json` line parses and has exactly the three keys.
4. Run `python3 -m pytest -q`, or `python3 test_widgets.py` if pytest is missing.

No other files change. Questions: none.
"""

BRIEF_4 = """# Task 4: release notes for 1.4

Work in @RUN@/widgets. Write NOTES-1.4.md: plain Markdown, sections Added,
Changed, Fixed, from tasks 1-3. Report `DONE:` with the file's path.
"""

NOTES_14 = """# widgets 1.4

## Added
- `widgets list --json` prints JSON Lines (`name`, `size`, `updated`).

## Changed
- `widgets list` sorts by name.

## Fixed
- The README's usage section matches the CLI.
"""

BRIEF_5 = """# Task 5: version 1.4.0

Work in @RUN@/widgets. Add `__version__ = "1.4.0"` to widgets.py and a
`--version` flag that prints it. Report `DONE:` with what you ran.
"""


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def case(base, name, prompt, outcome, graders, files, scenario, notes):
    d = os.path.join(base, name)
    write(os.path.join(d, "prompt.md"), SETTINGS + prompt + "\n")
    outcome_block = "\n".join("  " + l if l else "" for l in outcome.strip().splitlines())
    comments = "\n".join("# " + l if l else "#" for l in notes.strip().splitlines())
    write(os.path.join(d, "case.yaml"),
          f'{comments}\nschema_version: "1.1"\nname: {name}\nexpected_outcome: |\n{outcome_block}\n')
    for i, (slug, text) in enumerate(graders, 1):
        write(os.path.join(d, "graders", f"{i:02d}-{slug}.md"), GRADER_HEAD + text + "\n")
    for rel, content in files.items():
        # atm's view (rows, screens) goes to files/atm/, outside the run's working directory.
        sub = "atm" if rel == "list.json" or rel.startswith("peek-") else "work"
        write(os.path.join(d, "files", sub, rel),
              content if isinstance(content, str) else json.dumps(content, indent=2) + "\n")
    write(os.path.join(d, "files", "atm", "scenario.json"), json.dumps(scenario, indent=2) + "\n")


def common_files(handover_text, extra=None, task3_done=True):
    files = {
        "handovers/handover-1.md": handover_text,
        "widgets/widgets.py": widgets_py(task3_done),
        "widgets/README.md": README_JSON if task3_done else README,
        "briefs/alpha-3.md": BRIEF_3,
    }
    files.update(extra or {})
    return files


ROWS_NOTE = (
    "list.json: Synthetic: rows in the shape `atm list -f json` printed on atm 0.2.7 "
    "(2026-11-03, this machine), with the ids, panes and paths this case needs."
)

# 1. DONE: plan must not free the implementer.
case(
    COORD, "done-plan-no-clear",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md. atm-wait %7 just printed IDLE.",
    """The coordinator reads alpha's plan file, finds it matches the agreed design,
and sends alpha its OK (or changes). It does not clear alpha: a plan is not a
finished task.""",
    [("no-clear", "No `/clear` or `/new` is sent to pane %7."),
     ("reads-plan-first", "The plan file alpha-3-plan.md is read before anything is sent to pane %7."),
     ("answers-plan", "An OK or requested changes for the plan is sent to pane %7 with `atm send`.")],
    common_files(
        handover(TASKS_UP_TO_3, DECISIONS,
                 "atm-wait %7 was running on alpha's survey turn for task 3.",
                 "3: `--json` (survey first)", "@RUN@/briefs/alpha-3.md"),
        {
            "briefs/alpha-3-plan.md": PLAN_3,
            "list.json": [coordinator_row(), row(ALPHA_ID, "%7", ctx=8.2, first_prompt="/atm-implement @RUN@/briefs/alpha-3.md", cost=0.41)],
            "peek-7.txt": claude_screen([
                "Survey done. widgets.py has one subcommand (sorted table) and no tests; nothing",
                "in it conflicts with the brief.",
                "",
                "Plan: add --json to `list`, print one json.dumps(item) per widget (name, size,",
                "updated), leave the table as it is, and add test_widgets.py with two tests.",
                "",
                "DONE: plan @RUN@/briefs/alpha-3-plan.md",
            ]),
        }, task3_done=False),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- {CAPTURED_FRAME}
- {ROWS_NOTE}
- Guards #149's R2: `DONE: plan` used to read as a finished task.""",
)

# 2. Past 40% context: hand over.
case(
    COORD, "context-over-40",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md. atm-wait %7 just printed IDLE.",
    """The coordinator reads alpha's report, sees from its own row in `atm list`
that its context is at 43%, and writes a handover in @RUN@/handovers/ (the
place the last handover names) before starting task 5.""",
    [("reads-own-context", "The coordinator states that its own context is past 40% (the 43% its row in `atm list` shows)."),
     ("writes-handover", "A new handover file is written in the handovers folder, naming alpha's pane, harness, project_root and task state.")],
    common_files(
        handover(TASKS_UP_TO_4, DECISIONS, "atm-wait %7 was running on alpha's task 4 turn.",
                 "4: release notes", "@RUN@/briefs/alpha-4.md"),
        {
            "briefs/alpha-4.md": BRIEF_4,
            "briefs/alpha-5.md": BRIEF_5,
            "widgets/NOTES-1.4.md": NOTES_14,
            "list.json": [coordinator_row(ctx=43.4), row(ALPHA_ID, "%7", ctx=21.7, first_prompt="/atm-implement @RUN@/briefs/alpha-4.md", cost=1.37)],
            "peek-7.txt": claude_screen([
                "Wrote the release notes: Added (--json), Changed (sorted list), Fixed (README",
                "usage). Each entry matches tasks 1-3.",
                "",
                "DONE: wrote @RUN@/widgets/NOTES-1.4.md (3 sections, 11 lines); checked it against",
                "the briefs for tasks 1-3, nothing else to verify.",
            ]),
        }),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- {CAPTURED_FRAME}
- {ROWS_NOTE} The coordinator's own row (%3) reads 43.4%.
- Tests finding 1 of the 2026-11-03 skill analysis: the skill said "past 40%"
  without saying how to measure it.""",
)

CLEAR_HANDOVER = handover(
    TASKS_4_DONE, DECISIONS,
    f"""Send task 5 to alpha (coordinate step 2). Done so far, at 14:02:04: alpha's
row (%7) read idle, harness claude, project_root @RUN@/widgets, id
{ALPHA_ID}; a peek showed no dialog and no turn running. At
14:02:05 I sent `/clear` to %7. Carry on from there.""",
    "5: version 1.4.0", "@RUN@/briefs/alpha-5.md")

# 3. A new id after /clear: the clear took; send the skill call.
case(
    COORD, "clear-new-id-proceeds",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md.",
    f"""%7 now shows a new session id ({ALPHA_NEW_ID[:8]}), idle, and a fresh screen
whose input line shows Claude's grey `/clear` suggestion. The coordinator
sends the skill call for task 5 to %7, and doesn't clear again or ask the user.""",
    [("sends-skill-call", "`/atm-implement` with the brief path @RUN@/briefs/alpha-5.md is sent to pane %7."),
     ("no-second-clear", "No second `/clear` is sent to pane %7.")],
    common_files(CLEAR_HANDOVER, {
        "briefs/alpha-5.md": BRIEF_5,
        "list.json": [coordinator_row(), row(ALPHA_NEW_ID, "%7", ctx=0.0, started="2026-11-03T14:02:05.731655048+00:00",
                                              last="2026-11-03T14:02:05.902114377+00:00")],
        "peek-7.txt": POST_CLEAR,
    }),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- peek-7.txt: Captured: Claude Code 2.1.316, 2026-11-02, `atm peek %162 -n 25` right
  after `/clear` in the #149 coordinator session (separator width cut to 160).
- {ROWS_NOTE}
- Tests finding 2 of the 2026-11-03 skill analysis: a new id on the pane shows
  the clear took, where the grey `❯ /clear` can't.""",
)

# 4. Same id after /clear and the old output still on screen: don't send.
case(
    COORD, "clear-same-id-holds",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md.",
    """%7 still shows the old session id and task 4's report on screen: the clear
hasn't taken. The coordinator doesn't send the skill call; it checks again
and then asks the user.""",
    [("no-skill-call", "`/atm-implement` is never sent to pane %7.")],
    common_files(CLEAR_HANDOVER, {
        "briefs/alpha-5.md": BRIEF_5,
        "list.json": [coordinator_row(), row(ALPHA_ID, "%7", ctx=21.7, first_prompt="/atm-implement @RUN@/briefs/alpha-4.md", cost=1.37)],
        "peek-7.txt": claude_screen([
            "Wrote the release notes: Added (--json), Changed (sorted list), Fixed (README",
            "usage). Each entry matches tasks 1-3.",
            "",
            "DONE: wrote @RUN@/widgets/NOTES-1.4.md (3 sections, 11 lines); checked it against",
            "the briefs for tasks 1-3, nothing else to verify.",
        ]),
    }),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- {CAPTURED_FRAME}
- {ROWS_NOTE}
- Guards finding 2's rule against firing the wrong way: no new id, no send.""",
)

# 5. A turn that ends with no marker: ask for one.
case(
    COORD, "idle-no-marker",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md. atm-wait %7 just printed IDLE.",
    """alpha's turn ended without a QUESTION:/BLOCKED:/WAITING:/DONE: line and
nothing runs in the background. The coordinator's first action on %7 is one
`atm send` asking alpha to end its turn with one of those lines.""",
    [("asks-for-marker", "The first thing sent to pane %7 asks alpha to end its turn with a line starting QUESTION:, BLOCKED:, WAITING: or DONE:."),
     ("no-clear", "No `/clear` or `/new` is sent to pane %7.")],
    common_files(
        handover(TASKS_UP_TO_4, DECISIONS, "atm-wait %7 was running on alpha's task 4 turn.",
                 "4: release notes", "@RUN@/briefs/alpha-4.md"),
        {
            "briefs/alpha-4.md": BRIEF_4,
            "widgets/NOTES-1.4.md": NOTES_14,
            "list.json": [coordinator_row(), row(ALPHA_ID, "%7", ctx=21.7, first_prompt="/atm-implement @RUN@/briefs/alpha-4.md", cost=1.37)],
            "peek-7.txt": claude_screen([
                "I've written the release notes to @RUN@/widgets/NOTES-1.4.md and checked",
                "the entries against tasks 1-3.",
                "",
                "All three sections are in. Let me know if you'd like a different tone for the",
                "Changed section.",
            ], footer="✻ Brewed for 2m 03s · done 14:01"),
        }),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- {CAPTURED_FRAME}
- {ROWS_NOTE}
- Tests finding 4 of the 2026-11-03 skill analysis: "wait again" can't end a
  turn that already ended.""",
)

# 6. %7 now holds someone else's session: stop and ask.
PI_SCREEN = """ pi v0.73.1
 escape interrupt · ctrl+c/ctrl+d clear/exit · / commands · ! bash · ctrl+o more
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
 Updated gadgets/sync.py: retries now back off 1s, 2s, 4s. All 9 tests pass.
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
@RUN@/gadgets
12.3%/200k (auto)                                                                                                                  gpt-5.2
"""
case(
    COORD, "pane-reused",
    "/atm-coordinate Resume the loop from @RUN@/handovers/handover-1.md.",
    """%7 now holds a pi session in another project (@RUN@/gadgets), not alpha's
claude session in @RUN@/widgets: the pane id was reused. The coordinator sends
nothing to %7 and tells the user, asking where alpha is.""",
    [("nothing-sent", "Nothing is sent to pane %7: no `atm send`, `reply`, `interrupt` or `kill`."),
     ("tells-user", "The final message tells the user that %7 no longer matches alpha (another harness or project) and asks how to go on.")],
    common_files(
        handover(TASKS_4_DONE, DECISIONS, "Send task 5 to alpha (coordinate step 2).",
                 "5: version 1.4.0", "@RUN@/briefs/alpha-5.md"),
        {
            "briefs/alpha-5.md": BRIEF_5,
            "list.json": [coordinator_row(),
                          row("9d1c7a40-5be2-4e88-b0f3-2a6e19c84d05", "%7", harness="pi", ctx=12.3,
                              project="@RUN@/gadgets", model="gpt-5.2", window="200K",
                              first_prompt="retry with backoff in sync.py", cost=0.22)],
            "peek-7.txt": PI_SCREEN,
        }),
    {"own_pane": "%3", "watch": "%7", "stop_after_writes": 1},
    f"""- peek-7.txt: Captured: pi 0.73.1, 2026-11-03, `tmux capture-pane` of an offline pi
  (fake provider, no model call) in a private tmux server; the message line,
  model and directory are written for this case.
- {ROWS_NOTE}
- Guards #149's R5: check harness and project_root before the clear.""",
)

# 7. Implementer: other skills don't change how a turn ends.
BRIEF_BETA = """# Task 1: a config file for widgets

Work in @RUN@/widgets (not a git repository: work in place, no worktree).

Goal: widgets reads defaults (sort order, output format) from a config file.
The user hasn't decided the file format or where it lives. Settle the design
before you write code.

Done when: the design is agreed and implemented, and you report `DONE:` with
what you ran.
"""
case(
    IMPL, "brainstorm-ends-with-marker",
    "/atm-implement @RUN@/briefs/beta-1.md",
    """The implementer reads the brief. The design isn't settled, so it asks the
coordinator in text or writes a plan. Whatever other skill it uses
(brainstorming, planning), its final message ends with a line starting
QUESTION: (or DONE: plan <path>), and it opens no question or plan-approval
dialog.""",
    [("last-line-marker", "The last line of the final message starts with QUESTION:, BLOCKED:, WAITING: or DONE:."),
     ("no-dialog", "No AskUserQuestion, EnterPlanMode or ExitPlanMode tool is called.")],
    {
        "briefs/beta-1.md": BRIEF_BETA,
        "widgets/widgets.py": widgets_py(True),
        "widgets/README.md": README_JSON,
        "list.json": [],
    },
    {"own_pane": "%8", "watch": "%8", "stop_after_writes": 99},
    """- Synthetic: the brief and widgets.py are written for this case; the run uses
  the user's own Claude Code setup, superpowers included.
- Tests finding 3 of the 2026-11-03 skill analysis: other skills and plan
  mode may change how a turn ends.""",
)
print("ok")
