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

- Pi has no native tool approvals, but extension confirm/select/input/editor
  dialogs and core dialogs replace the editor. Recognized hints/reload text
  produce Other, with input/turn Unknown. Unrecognized extension UI is not
  generally distinguishable from a draft; arbitrary custom editors remain
  outside this parser's supported contract.
- Free-text and multiline drafts have synthetic tests only, not real captures.
  **F2:** plain capture drops trailing ASCII spaces, so whitespace-only drafts
  can read Empty. Space-preservation tests do not establish end-to-end safety;
  styled capture with `-N` is deferred to U8d.
- No cursor or attributes were captured (`capture-pane -p`). Real replays use
  None; supplying cursor metadata currently returns all Unknown. Cursor,
  control, truncation and draft mutations are explicitly Synthetic tests.
- Fixed borders, labels and separators are exact; versions, model/effort,
  numeric cost/counters/context, cwd/branch and resource lists are values.
  `↑`, `↓`, `R`, `W` counters are independently optional, as is `CH<pct>%`.
  Cost may be absent; `(sub)` and `(auto)` are optional values, not chrome.
  Context may be unknown (`?`), in which case freshness is Unknown. The model
  may lack effort, use `thinking off`, include a provider or routed model,
  or be omitted when the pane fits only the complete left stats. All ten frames
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

## Rendered footer variants (not real captures)

`screen_pi-footers_1.5.0.txt` has 18 rows produced by pi 1.5.0's `footer.js`,
labelled Synthetic. It covers default, cache-write, independent arrows, unknown
context, thinking off, non-reasoning, no cost, API cost without subscription,
no auto, provider prefix, routed model, no model, truncated stats (Unknown),
stats-only narrow pane, context over 100%, experimental marker and zero context
window. Rendering used supplied plain session data, no CLI or model; SGR was
removed and trailing spaces trimmed. Tests replace only the footer/cwd in the
real fresh screen and assert the four facts and conservative zero-usage rule.

All source-rendered listed forms are supported; a clipped context anchor,
partial effort/route, unknown left marker or extension-status row still refuses.
The fixed two-space minimum alignment distinguishes provider prefixes from
unknown left markers. Startup chrome is located by the first nonblank title
and help lines, not row numbers. A cwd of exactly `~` is supported.

## Rendered dialog regressions (not real captures)

Seven `screen_pi-dialog-<name>_1.5.0.txt` fixtures contain editor-slot renders:
select, confirm, missing-cwd and summarize from `extension-selector.js`, input
from `extension-input.js`, editor from `extension-editor.js`, and reload from
`interactive-mode.js`'s `reloadBox` construction. Each carries
`Synthetic: rendered from pi 1.5.0's <file>; no real capture`.

These were generated by importing the installed 1.5.0 components, initializing
the dark theme and rendering at 120 columns (40 terminal rows for the nested
editor). No CLI, model, dialog callback or timer ran. Fake cursor markers were
resolved away as the TUI does before terminal output, SGR removed, and trailing
ASCII spaces trimmed to match plain capture. Titles/options are synthetic.
Tests splice the body (not the label) into the real idle screen and require
unknown input/turn/freshness and Other or Unknown dialog, including custom
key-name variants. The reload paragraph is joined across wrapped rows.

A synthetic draft ending in a full-width rule plus an empty row must not read
Empty. Earlier same-width rules make the border selection ambiguous unless
the preceding region is recognized startup-only chrome (e.g. the update box).
This conservatively refuses some conversations containing full-width rules;
plain text cannot reliably distinguish those rules from pasted editor borders.

No RPC method is added. Captured success and synthetic refusal tests cover
parser conformance; static clear/launch policy has exact-value tests.
