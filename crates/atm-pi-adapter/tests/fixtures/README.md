# Pi screen fixtures

Ten distinct frames cover all 15 raw U5 pi screens (40 rows, 120 columns).
Existing JSONL fixtures retain the provenance in the crate docs. The screen
parser's `captured_screens` table checks all four facts for every frame.

Every row below has this provenance:
**Captured: pi 1.5.0, 2026-11-14, U5 private-tmux capture; sanitized.**

Source `pi-<name>.txt` maps to `screen_pi-<name>_1.5.0.txt`.
Duplicate names were byte-identical before sanitization. No `.prompt.txt`
presentation excerpt is used.

| Source representative | Identical source names (without `.txt`) | Observed meaning |
|---|---|---|
| `pi-startup.txt` | `pi-restarted` | Empty, idle, fresh startup layout |
| `pi-trust-startup.txt` | — | Empty, idle, fresh startup; update notice, NOT a trust dialog |
| `pi-arrival.txt` | `pi-clear-poll-00` | Empty, idle, ordinary reply |
| `pi-after-clear.txt` | — | Empty, idle, already contains the next reply; not fresh |
| `pi-escape-5.txt` | `pi-escape-05`, `pi-escape-2` | Empty, idle after interruption; no restored draft |
| `pi-before-escape.txt` | `pi-escape-0` | Empty composer with Working upper border; turn running |
| `pi-clear-poll-01.txt` | — | Empty, idle, fresh new-session layout |
| `pi-shell.txt` | — | Historical editor above shell; all Unknown |
| `pi-final-shell.txt` | — | Historical startup editor above shell; all Unknown |
| `pi-trust-final-shell.txt` | — | Historical update notice/editor above shell; all Unknown |

## Sanitization

Only these substitutions were made and source-to-fixture diff reviewed:

- Private run root -> `/tmp/u8a-fixture`, including cwd/socket/session paths.
- Capture helper -> `/tmp/u8a-fixture/bin/capture.py`.
- Agent run tags -> `fixture/pi` (and `fixture/pi-trust`); prompt markers
  -> `fixture1` / `fixture2`, with unchanged character counts.
- Resume UUID -> `00000000-0000-4000-8000-000000000001` consistently.
- Home prefixes would become `/home/user`; none occurred in these frames.
- Shortened lines were right-padded to preserve original character counts.
  Intentional trailing spaces are capture geometry, not editor drafts.

No rows were removed, wrapped or added. Borders, blank input rows, Unicode
and footer columns are unchanged. No real IDs or original local paths remain.

## Missing states and limits

- Pi has no native approval dialogs. No extension/custom dialog shape was
  captured; unsupported UI remains Unknown, not inferred safe.
- Free-text, whitespace-only and multiline drafts have synthetic nonempty
  safety tests only, not positive real-harness evidence.
- No cursor or attributes were captured (`capture-pane -p`). Real replays use
  None; supplying cursor metadata currently returns all Unknown. Cursor,
  control, truncation and draft mutations are explicitly Synthetic tests.
- Fixed borders, labels and separators are exact; versions, model/effort,
  numeric cost/counters/context, cwd/branch and resource lists are values.
  Optional cache tokens `R<count>` and `CH<pct>%` are accepted. All ten frames
  of the braille spinner are recognized in the Working upper border.
  Unknown structure, rather than a new value, refuses observation. Zero cost,
  context and counters remain required for fresh-layout evidence.
- Startup help says “escape interrupt” while idle. Activity comes only from
  the current editor's upper border. The new-session notice persists after a
  later reply; it is not fresh evidence by itself.
- Present is fresh layout evidence, not proof a particular clear completed.
  Clearing an already-fresh editor or inside a Git worktree is unverified.
- A copied full screen is indistinguishable from a live one. Consumers own
  current viewport, process/pane identity, copy-mode and temporal checks.

## Additional coordinator captures

Every row below has this provenance:
**Captured: pi 1.5.0, 2026-11-14, coordinator read-only tmux capture-pane -p; sanitized.**

| Source | Fixture | Facts |
|---|---|---|
| `pi-1.5.0-idle-pane61.txt` | `screen_pi-extra-idle-1_1.5.0.txt` | Empty, idle, no dialog, not fresh |
| `pi-1.5.0-idle-pane62.txt` | `screen_pi-extra-idle-2_1.5.0.txt` | Empty, idle, no dialog, not fresh |
| `pi-1.5.0-idle-pane64.txt` | `screen_pi-extra-idle-3_1.5.0.txt` | Empty, idle, no dialog, not fresh |
| `pi-1.5.0-working-pane65-1.txt` | `screen_pi-extra-working-1_1.5.0.txt` | Empty, Running (`⠹`), no dialog, not fresh |
| `pi-1.5.0-working-pane65-2.txt` | `screen_pi-extra-working-2_1.5.0.txt` | Empty, Running (`⠧`), no dialog, not fresh |
| `pi-1.5.0-working-pane65-3.txt` | `screen_pi-extra-working-3_1.5.0.txt` | Same frame as working-1 |

These live viewports have no startup header: ordinary facts depend on the
complete current bottom layout, not historical header visibility. All six
have nonzero cost/cache usage and a different model/effort from U5. They
are replayed in `coordinator_captures_with_cache_and_spinner_variants`.

Additional sanitization: every nonblank transcript row above the upper border
was replaced with a redaction marker, padded to its original character count;
blank rows remain blank. Cwd/branch becomes `~/project (topic)`, padded.
No border, composer, usage/model footer or spinner was altered. Row counts
and each row's character count are preserved, with no wrapping. Conversation
text, commit IDs and local paths are not retained. These changes remove no
structure used for ordinary editor facts; no fresh evidence is inferred from
redacted history. Synthetic value-variation and structural-damage tests are
labelled as such and supplement, not replace, the real replays.

No RPC method is added. Captured success and synthetic refusal tests cover
parser conformance; static clear/launch policy has exact-value tests.
