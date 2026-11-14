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
