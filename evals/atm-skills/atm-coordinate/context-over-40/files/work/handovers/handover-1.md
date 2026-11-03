# Coordinator handover: widgets 1.4

Written 2026-11-03 14:00 by the coordinator in pane %3 (claude). Handovers go
in @RUN@/handovers/ (the user's choice); briefs in @RUN@/briefs/.

## Implementers

| Name | Pane | Harness | project_root | Worktree | Task | Brief |
|---|---|---|---|---|---|---|
| alpha | %7 | claude | @RUN@/widgets | @RUN@/widgets | 4: release notes | @RUN@/briefs/alpha-4.md |

## Tasks

1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. Done: `--json` for `widgets list` (alpha, reviewed).
4. In progress (alpha): release notes for 1.4 in @RUN@/widgets/NOTES-1.4.md.
   Brief @RUN@/briefs/alpha-4.md.
5. Not started: bump the version to 1.4.0. No brief yet.

## Decisions

- `--json` prints JSON Lines: one object per widget with `name`, `size` and
  `updated`. The default table output doesn't change. (user, 14:20 yesterday)
- Release notes are plain Markdown, three sections: Added, Changed, Fixed.
- No implementer pushes or tags; the user releases.

## Open questions

None.

## Next step

atm-wait %7 was running on alpha's task 4 turn.
