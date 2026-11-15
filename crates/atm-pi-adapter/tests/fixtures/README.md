# Pi screen fixtures

## Staged message references (P3)

Captured: pi 1.5.0, 2026-11-14, sealed loopback-only private tmux
`capture-pane -p -e -N` (styled) and `capture-pane -p -N` (plain).
Each row maps `<source>/B-pi-<stem>.{styled-N,plain-N}.txt`
to `screen_p3_<stem>_1.5.0.{styled,plain}.txt`. Each file also carries its
individual capture method and state in its provenance line.

| Source | Stem (one styled/plain pair per row) | Evidence |
|---|---|---|
| b-wide | 80x40-r0-C-u-empty | Same-run empty editor |
| b-wide | 80x40-r0-C-u-staged | Short reference, single row |
| b-wide | 80x40-r0-C-u-after-C-u | First clear observation |
| b-wide | 80x40-r0-C-u-cleared | Settled clear observation |
| b-wide | 120x40-r1-C-u-empty | Same-run empty editor |
| b-wide | 120x40-r1-C-u-staged | Long reference, single row |
| b-wide | 120x40-r1-C-u-after-C-u | First clear observation |
| b-wide | 120x40-r1-C-u-cleared | Settled clear observation |
| b-narrow-corrected | 28x40-r1-C-u-empty | Same-run narrow empty editor |
| b-narrow-corrected | 28x40-r1-C-u-staged | Long reference, real two-row wrap |
| b-narrow-corrected | 28x40-r1-C-u-after-C-u | First narrow clear observation |
| b-narrow-corrected | 28x40-r1-C-u-cleared | Settled narrow clear observation |
| b-wide | 120x40-r1-Escape-empty | Independent Escape-run baseline |
| b-wide | 120x40-r1-Escape-staged | Independent Escape-run staged draft |
| b-wide | 120x40-r1-Escape-after-Escape | Escape leaves the draft intact |

Only private paths, including clipped path fragments, become equal-length
`/tmp/xxx…` placeholders. Rows, spaces, ANSI sequences and editor text stay
unchanged; tests remove only the added provenance line. Sequential plain and
styled observations are not an atomic pair. No Enter was sent in this probe.

`tests/staged_reference.rs` checks both reference lengths at wide widths,
wrong references, plain-input refusal, cleared negatives and explicitly
Synthetic changed-token/outside-editor mutations. The wrapped exact match
currently returns None even though its control facts read Draft / NotRunning
/ None, with control dialog None. One Escape does not clear the reference:
its wide frame still matches and must not be called a cleared negative.
Matching alone is not authorization to submit.

## Staged clear command and typed popup (P5)

Captured: pi 1.5.0, 2026-11-14, P5 sealed loopback-only private tmux 3.8
`capture-pane -p -e -N` (styled) and `-p -N` (plain).
Each row maps `a-pi-cleanup/A-pi-<size>-<state>.{styled-N,plain-N}.txt`
to `screen_p5_<size>-<state>_1.5.0.{styled,plain}.txt`.

| Size | State |
|---|---|
| 80x40 | staged |
| 80x40 | typed-staged |
| 80x40 | typed-popup |
| 80x40 | typed-first-after-down |
| 80x40 | typed-popup-remains |
| 80x40 | typed-cleanup-escape |
| 100x40 | staged |
| 100x40 | typed-staged |
| 100x40 | typed-popup |
| 100x40 | typed-first-after-down |
| 100x40 | typed-popup-remains |
| 100x40 | typed-cleanup-escape |
| 120x40 | staged |
| 120x40 | typed-staged |
| 120x40 | typed-popup |
| 120x40 | typed-first-after-down |
| 120x40 | typed-popup-remains |
| 120x40 | typed-cleanup-escape |

Only private run roots become equal-width generic `/tmp/xxx…` paths. All
terminal rows, ANSI sequences and spaces remain; tests strip only the added
provenance line. Distinct samples are retained even when bytes coincide.
ATM pastes `/new`: P5's paste had no completion popup and matches exactly,
with control None and delivery Draft / NotRunning / None. Typing the same
text opens a colored `→ new` candidate between the editor border and cwd
footer; this human path deliberately remains control Unknown with no staged
match. Its inverse cell is the editor cursor, not candidate selection.

Ctrl-U removed the typed draft but left the candidate (`typed-popup-remains`),
still Unknown. Escape removed it (`typed-cleanup-escape`), restoring a Styled
Empty editor with control None. This is captured probe cleanup, not permission
for production cleanup or retry. Plain staged matching always returns None;
plain cleanup reads Draft, not Empty. `tests/staged_clear.rs` pins these facts
and labels editor/text mutations Synthetic. Conversation is outside this unit.

## Lifecycle control observations (B0)

Captured: pi 1.5.0, 2026-11-14, sealed P1 private tmux
`capture-pane -p -e -N`. New
`screen_control_p1_B-conversation-120x40_1.5.0.txt` maps to
`B-conversation-120x40.styled-N.txt`, paired with `.plain.txt` and `.meta.json`.
Only the private run root becomes an equal-width `/tmp/xxx…` path; all rows,
attributes, spaces and composer content remain. Strip the provenance label at
load. Other control tests reuse the existing styled P1 fixtures below.

No startup dialogs appeared: the control observer emits None only for existing
recognized editors, Unknown otherwise. Conversation is independent of draft/
turn facts: validated transcript blocks beyond recognized startup prove visible
history (see the stricter startup-suffix contract below).
P1's scrolled working/draft screens have separate indented transcript blocks
and nonzero usage; neither usage nor an old new-session notice alone proves
conversation or freshness. A draft alone is not conversation. Unknown custom
UI, trust-gated project startup and permission extensions remain unverified.
Mutations are Synthetic; legacy observation and staged-check results do not change.

## Styled fixtures (U8d)

Captured: pi 1.5.0, 2026-11-14, P1 private tmux 120x40
`capture-pane -p -e -N`.
`screen_styled_p1_<state>_1.5.0.txt` maps to `<state>.styled-N.txt`;
`<state>.meta.json` holds source geometry/cursor metadata. Provenance headers
are removed only at test load. Only the private run root outside composers was
replaced by an equal-width `/tmp/xxx…` token. No SGR, rows, spaces or composer
content was normalized. Tests cover blank startup/new session, one/multiline
drafts, working/queued input, selector and retry countdown. The held retry
request has a Working border; only the countdown is an unknown border.

Styled coverage supersedes the plain-only limitations below. Blank/padding
stays Empty with its accepted whitespace-only-draft collision. Tmux 3.8/3.6
probes show -N keeps allocated padding as well as authored spaces. Safety
requires exact staged-draft verification before Enter, refusing unknown.
Leading spaces survive comparison; multiline reconstruction remains unknown.
Trailing ASCII spaces are chrome; expected text must have no trailing whitespace.
P1 `C4-one-draft`, `B-0-event-pasted` and `C4-one-pasted` pin matching real padded
drafts. Faint text is not typed input, and a match alone does not prove Enter
submits rather than interacting with a popup.
Captured: pi 1.5.0, 2026-11-14, sealed private-tmux command probe: `/new   `
followed by Enter emitted shutdown/start with reason new and a changed ID,
not input; no API request occurred.
Synthetic adversarial cases are labelled in `tests/styled_screen.rs`.

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

Fourteen `screen_pi-dialog-<name>_1.5.0.txt` fixtures contain editor-slot renders
at 120 columns and corresponding `-narrow` renders at 40 columns:
select, confirm, missing-cwd and summarize from `extension-selector.js`, input
from `extension-input.js`, editor from `extension-editor.js`, and reload from
`interactive-mode.js`'s `reloadBox` construction. Each carries
`Synthetic: rendered from pi 1.5.0's <file>; no real capture`.

These were generated by importing the installed 1.5.0 components, initializing
the dark theme and rendering at 120/40 columns (40 terminal rows for the nested
editor). No CLI, model, dialog callback or timer ran. Fake cursor markers were
resolved away as the TUI does before terminal output, SGR removed, and trailing
ASCII spaces trimmed to match plain capture. Titles/options are synthetic.
Tests splice the body (not the label) into the real idle screen and require
unknown input/turn/freshness and Other or Unknown dialog, including custom
key-name variants. Hint actions and the reload paragraph are recognized across
wrapped rows; a draft imitating dialog action words conservatively holds too.

A synthetic draft ending in a full-width rule plus an empty row must not read
Empty. In Styled input every earlier same-width rule must belong to a complete
update notice with its captured faint-label proof (see below). An otherwise
valid startup region never excuses an unexplained rule. Plain retains its
legacy startup-only exception: it cannot supply that attribute proof. Other
historical full-width rules remain ambiguous and conservatively refuse.

No RPC method is added. Captured success and synthetic refusal tests cover
parser conformance; static clear/launch policy has exact-value tests.

## Startup conversation evidence (U8i)

Captured: pi 1.5.0, 2026-11-14, P3/P5 sealed loopback-only private tmux 3.8
`capture-pane -p -e -N` (Styled) and `capture-pane -p -N` (Plain).
`control_startup/manifest.json` maps all 260 original relative capture paths to
50 deduplicated, version-labelled fixture bodies. Each row records original and
sanitized SHA-256, geometry, format, provenance and pre-fix independent facts,
control dialog and conversation value. Bodies have no added provenance header.
Only private run roots become equal-width `/tmp/xxx…` paths, including clipped
cwd rows; rows, ANSI, spaces and editor text are otherwise byte-preserved.

Coverage includes P5 `a-initial`, `a-pi-typed`, `a-pi-cleanup` at 80/100/120
columns and P3 `b-wide`, `b-narrow`, `b-narrow-corrected` at 28/80/120 columns
(no P3 100-column capture). Drafts, first-after-input, stale popups and cleanup
are retained, not just empty editors. All are pre-first-turn frames. Before the
fix 104 wide frames falsely reported conversation from fd download status; all
260 must now report false, without changing any independent fact or dialog.
Retained Plain padding can report Draft; styled empty editors remain Empty.
Wrapped startup still does not prove freshness, and popup Unknown stays Unknown.

After recognized startup, positive evidence requires at least two nonempty,
blank-separated transcript blocks, **every** block singly indented and free of
warning/notice/queued-output rows. Scrolled-history evidence is unchanged.
Nonzero counters alone are not history. Deliberately, a single visible block or
real history following unsupported startup output may remain false: absence of
positive evidence is not freshness and must not replace session-identity proof.
Arbitrary custom UI imitating two transcript-shaped prose blocks is unverified.
The existing P1 conversation/scrolled positives and zero-usage prompt/reply
regression remain unchanged. `control_startup.rs` labels Synthetic mutations of
values, spacing, suffix blocks, anchors, footer, borders and ANSI; these are not
additional captures. Live post-build harness/daemon orchestration is unverified.

## Update-notice/editor boundary (clear startup)

Captured: pi 1.5.0, 2026-11-15, sealed private tmux 3.8
`capture-pane -p -e -N`; startup and existing short-turn captures.
Captured: pi 1.5.2, 2026-11-15, sealed private tmux 3.8
`capture-pane -p -e -N`; offline/resource startups, no input or prompt.

All `clear_*_pi_<version>.txt` bodies are verbatim captures with no added header,
normalization or path substitution. They contain only private run-dir paths.
`tests/clear_startup_notice.rs` records provenance beside each replay.

| Fixture stem (`clear_<stem>_pi_1.5.0.txt`) | Source / state |
|---|---|
| `post_turn` | E2 before-clear captures, byte-identical pair; empty editor after a short turn |
| `e2_startup` | Same E2 session before its first turn; full header, resources and update |
| `rehearsal` | Independent short-turn refusal at 150 columns, header scrolled off |
| `startup_224x16`, `startup_224x12`, `startup_120x16`, `startup_80x16`, `startup_60x20`, `startup_40x20` | Separate real startup-only processes; clipped/wrapped header and complete update |
| `startup_224x48` | Full-header startup with update, after fd was installed |
| `startup_fd_status` | First 224x48 startup, fd downloading/installed status plus update |
| `offline-wide`, `offline-narrow` | No update notice, 224x48 / 40x40 |
| `resources-wide`, `resources-narrow` | Loaded context, skill and prompt, 224x48 / 40x40 |
| `changelog-condensed-wide`, `changelog-condensed-narrow` | Unread core changelog, 224x48 / 40x40, nonfaint body; already Unknown |
| `changelog-detailed-narrow` | Unread expanded core changelog, 40x60; unsupported underline SGR, already Unknown |

The four offline/resource stems also have `_pi_1.5.2.txt` captures from the
installed 1.5.2 CLI. Startup was synchronized on complete footer, expected
resources/notice and the post-init tmux warning, never a fixed delay or resize.

Cause: the update's full-width rules equal the editor lower border. Requiring
*all history* to be startup-only made an idle editor Unknown after a short turn,
with a cropped header, or even after fd download status. Styled now accounts for
**every** colliding rule individually: only a complete `Update Available` notice,
with the observed faint New-version and Changelog labels, can explain it.
Instruction wrapping at 40 columns is reconstructed only inside that complete
notice. Decoded UTF-8 offsets include retained padding; Unicode history does not
shift the attribute proof. Other borders, unsupported escapes, popups and missing
proof still refuse. No changes to freshness/conversation criteria or Plain.

The installed 1.5.2 normal notice markup and editor border rendering match
1.5.0. The custom editor is identical; the base editor's only difference is
paste-marker renumbering. The optional release-note Markdown rendering changed,
but that notice form was already unsupported and stays Unknown.

Known limits intentionally remain refused: condensed/expanded unread core and
extension changelogs, package-update notices, Update Available with a release
note, experimental first-time setup and startup dialogs. Their appearance is
not an empty-editor proof. Custom themes without the captured faint labels and
arbitrary custom UI imitating both chrome and styles remain unsupported. No
text-only exception or submitting Enter is inferred from a notice match.

Synthetic regressions cover staged `/new` exactness, running/typed/dialog
editors, a valid notice plus an unexplained pasted rule, nonfaint notice-shaped
drafts, missing bounds/labels/attributes, damaged footer/ANSI and varied values
or history length. Restoring the old guard makes the real positive fail; changing
all-rule coverage to any-rule, or dropping faint proof, admits unsafe composers
and fails the dedicated negatives. No new model turn was needed for this fix.
