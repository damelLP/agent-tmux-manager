# Renderer sidecars

These are unchanged copies of selected U5b captures. Plain text is reused from
`crates/atm-{claude,codex}-adapter/tests/fixtures/`, not duplicated here.
`screens.json` pairs each source, SGR sidecar, cursor sidecar and native geometry.

- Captured: Claude Code 2.1.325, 2026-11-14, U5b private tmux 100x30,
  `capture-pane -e -p` and `display-message -p cursor_x,cursor_y`.
- Captured: codex-cli 0.167.0, 2026-11-14, U5b private tmux 100x30,
  `capture-pane -e -p` and `display-message -p cursor_x,cursor_y`.

Each file also retains its original provenance label. Sources already sanitized
home/config/run paths to `~`; these copies make no further substitutions.
Filenames add the harness version. Families selected:

| Harness | Families | States |
|---|---|---|
| Claude | rich-enabled, poor | fresh, different draft, identical suggestion draft, erased, after clear |
| Codex | rich, poor | fresh, different draft, identical placeholder draft, erased, after clear |

The renderer removes only the metadata label. It retains all SGR, NBSP, rows and
spaces, including styled cursor spaces that the paired plain `capture-pane -p`
dropped. Pair validation therefore permits trailing **ASCII** space differences,
not differences in any other characters or row structure. An identical draft can
carry different attributes from a placeholder; those attributes are not inferred
or fabricated from text. Cursor files are evidence for later styled-observer work,
not current parser inputs.

U5 and coordinator plain captures in the catalog remain unstyled; they have no
SGR sidecar. Their provenance and sanitization are in their adapter fixture
READMEs. Coordinator pi variants use the captured 116-column full-width border
and retained viewport row count; this is source replay, not arbitrary resizing.
No new real harness or model was invoked to produce this catalog.

## P1 plain captures

Captured: Claude Code 2.1.325, codex-cli 0.167.0, pi 1.5.0, 2026-11-14,
P1 sealed private tmux `capture-pane -p`. The `p1_` filename includes the original
capture stem, geometry and harness version. Each file has its own provenance label.
Run-directory prefixes were replaced by `/fixture/` plus equal-length `x` padding,
so all original columns, line wrapping, text outside paths, and trailing spaces
remain unchanged. No identifiers or local infrastructure paths were added.

| Harness | Original stems | Meaning |
|---|---|---|
| Claude | A80/A120/A200-default and -selected | Negative-default trust and selected Yes, 80/120/200x40 |
| Codex | a16-default-fresh-folder-trust, a17-default-fresh-hook-trust | Folder access and Hooks need review, 100x24 |
| Codex | a04-review-detail | Hook list, 100x32; this early probe's hook command was not executable, so the screen is evidence only, not proof of hook delivery |
| Codex | b31-nodaemon-clear-dialog, b35-default-repo-clear-dialog | Current/new worktree choice, 100x32 and 100x24 |
| Codex | b13-after-completed-new | Blank clear transition, 100x32; not guaranteed to be sampled |
| Codex | c16-permission-visible | Real three-option approval dialog, 100x32 (not U5's misleading permission filename) |
| pi | C4-one-draft, C4-multi-draft | Real draft frames, 120x40; names describe probe scenarios, not necessarily visible draft line count |

These P1 entries currently use plain output. A later change can attach a validated
`style`/`cursor_source` pair in the catalog without changing the runtime or model.
Synthetic: arbitrary runtime drafts/folders, numbered selection movement where
no selected capture exists, and uncaptured viewport fit in `scene.py`. Source-only
replay uses `render_screen` unchanged; it never applies those mutations.
