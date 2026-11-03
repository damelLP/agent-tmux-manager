# Coordinator handover: widgets 1.4

Written 2026-11-03 14:00 by the coordinator in pane %3 (claude). Handovers go
in @RUN@/handovers/ (the user's choice); briefs in @RUN@/briefs/.

## Implementers

| Name | Pane | Harness | project_root | Worktree | Task | Brief |
|---|---|---|---|---|---|---|
| alpha | %7 | claude | @RUN@/widgets | @RUN@/widgets | 5: version 1.4.0 | @RUN@/briefs/alpha-5.md |

## Tasks

1. Done: `widgets list` sorts by name (alpha, reviewed).
2. Done: README usage section (alpha, reviewed).
3. Done: `--json` for `widgets list` (alpha, reviewed).
4. Done: release notes for 1.4 (alpha, reviewed).
5. Next: bump the version to 1.4.0. Brief written: @RUN@/briefs/alpha-5.md.

## Decisions

- `--json` prints JSON Lines: one object per widget with `name`, `size` and
  `updated`. The default table output doesn't change. (user, 14:20 yesterday)
- Release notes are plain Markdown, three sections: Added, Changed, Fixed.
- No implementer pushes or tags; the user releases.

## Open questions

None.

## Next step

Send task 5 to alpha (coordinate step 2). Done so far, at 14:02:04: alpha's
row (%7) read idle, harness claude, project_root @RUN@/widgets, id
6feb9cc9-2d41-4f7a-8b3e-5c0a9e1d7f22; a peek showed no dialog and no turn running. At
14:02:05 I sent `/clear` to %7. Carry on from there.
