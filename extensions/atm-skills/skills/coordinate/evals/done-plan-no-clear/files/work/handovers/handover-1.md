# Coordinator handover: widgets 1.4

Written 2026-11-03 14:00 by the coordinator in pane %3 (claude). Handovers go
in @RUN@/handovers/ (the user's choice); briefs in @RUN@/briefs/.

## Implementers

| Name | Pane | Harness | project_root | Worktree | Task | Brief |
|---|---|---|---|---|---|---|
| alpha | %7 | claude | @RUN@/widgets | @RUN@/widgets | 3: `--json` (survey first) | @RUN@/briefs/alpha-3.md |

## Tasks

1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. In progress (alpha): `--json` for `widgets list`. Brief @RUN@/briefs/alpha-3.md
   asks for a survey and plan first.

## Decisions

- `--json` prints JSON Lines: one object per widget with `name`, `size` and
  `updated`. The default table output doesn't change. (user, 14:20 yesterday)
- Release notes are plain Markdown, three sections: Added, Changed, Fixed.
- No implementer pushes or tags; the user releases.

## Open questions

None.

## Next step

atm-wait %7 was running on alpha's survey turn for task 3.
