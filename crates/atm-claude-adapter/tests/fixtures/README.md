# Claude screen fixtures

## Custom statusLine region (stage 1)

Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -p -e, custom statusLine.

`screen_styled_s1_status-<width>-<state>_2.1.325.txt` contains six real
unprompted captures, at 80, 116 and 160 columns, each fresh and with the
unsubmitted draft `[m42] Read: atm inbox --id m42`. The corresponding source
stems are `narrow`, `medium` and `wide`, followed by `-fresh.styled` or
`-draft.styled`. All have a complete supported startup header. No Enter was
sent and no model turn ran. Initial suggestions were disabled in the sealed
settings; the fresh editor is actually blank. The subsequent draft captures
also contain a late auto-mode disclaimer that this adapter does not recognize
as idle history: their input is Draft, but their turn remains Unknown, exactly
as in a status-free twin. Exact staged text is not permission to submit it.

The private HOME's statusLine command printed these neutral static values:

| Width | Output |
|---|---|
| 80 | `[neutral] load ▰▰▰▱ 83% Demo Model (low)` |
| 116 | `[sample] ctx ▰▰▱▱ 42% Haiku 5.5 (medium)` |
| 160 | `[fixture] ctx ▰▱▱▱ 7% Fable 5.2 (high)` |

Claude adds two leading spaces and places this one row immediately after the
lower editor rule, before the mode footer. The adapter recognizes the slot,
not these values. The rest of that region may contain only blank padding.
Additional/displaced rows, wrapped status output and unknown footers remain
unsupported. Values printed in the slot never count as staged editor text.

The probe used a private tmux 3.8 server, empty tmux config, sealed HOME,
allowlisted environment and a loopback-only network namespace with no routes;
no real credentials, daemon or user hooks were loaded. Only the private
run-root prefix in the header becomes an equal-width generic `/tmp/xxx…`
prefix. Otherwise the captured bytes, rows, columns, attributes and input are
unchanged; the added provenance line is not a terminal row. No owner username,
hostname, home path or account identifier is present. Composed running,
conversation, style and modal cases in tests are explicitly Synthetic, not
new real state captures.

### Slash-menu, color and truncation follow-up

Captured: Claude Code 2.1.325, 2026-11-15, sealed tmux capture-pane -p -e -N, custom statusLine.

`screen_styled_s1_status-clear-{80,116}_2.1.325.txt` maps to
`slash-{80,116}-clear.styled-N`. `/clear` was pasted but never submitted.
In these real fullscreen frames the selected slash menu is **above the upper
editor rule**; the status row remains below the lower rule, and the normal
auto-mode footer stays. The 80-column description is truncated, while the
116-column description wraps. The ordinary staged/control readers already
recognize the editor exactly; the older below-editor completion grammar is
unchanged. Tests also remove only the status row in a labelled Synthetic twin.

`screen_styled_s1_status-color-{88,160}_2.1.325.txt` maps to
`color-{88,160}-fresh.styled-N`. The neutral status command prints SGR
`38;5;246`, `38;5;151`, `38;5;66` and reset `39` around the same text. At 160
columns it is complete; at 88 Claude truncates that single row with `…` rather
than wrapping it. Both real blank editors are Empty/NotRunning with a fresh
layout. Color, labels, percentages, model names and truncation text never
supply readiness evidence.

The same private HOME/config, loopback-only/no-route namespace, fake
credentials and verified private tmux isolation described above were used.
No model turn or Enter was sent. Only the private run-root prefix becomes an
equal-width generic `/tmp/xxx…` prefix; every other byte, including retained
grid padding, is unchanged apart from the provenance line. No owner identity
or infrastructure path is present.

**Known limit:** underline (`4`/`24`) and strikethrough (`9`/`29`) in a status
row remain unsupported by the strict styled-screen decoder. Synthetic
mutations test that facts/control stay Unknown and staging stays None.
These decorations were not present in the real colored captures. There is
no decoder relaxation here, and multi-row status output remains unverified.

## Staged message references (P3)

Captured: Claude Code 2.1.325, 2026-11-14, sealed loopback-only private tmux
`capture-pane -p -e -N` (styled) and `capture-pane -p -N` (plain).
Each row maps `b-claude-confirm/B-claude-<stem>.{styled-N,plain-N}.txt`
to `screen_p3_<stem>_2.1.325.{styled,plain}.txt`. Each file also carries its
individual capture method and state in its provenance line.

| Stem (one styled/plain pair per row) | Evidence |
|---|---|
| 80x40-r0-C-u-empty | Same-run empty editor |
| 80x40-r0-C-u-staged | Short reference, single row |
| 80x40-r0-C-u-after-C-u | First clear observation |
| 80x40-r0-C-u-cleared | Settled clear observation |
| 120x40-r1-C-u-empty | Same-run empty editor |
| 120x40-r1-C-u-staged | Long reference, single row |
| 120x40-r1-C-u-after-C-u | First clear observation |
| 120x40-r1-C-u-cleared | Settled clear observation |
| 28x40-r1-C-u-empty | Same-run narrow empty editor |
| 28x40-r1-C-u-staged | Long reference, real two-row wrap |
| 28x40-r1-C-u-timeout | One Ctrl-U leaves the first visual row |
| 28x40-r1-C-u-second-C-u-cleared | Second Ctrl-U clears the remainder |
| 120x40-r1-Escape-empty | Independent Escape-run baseline |
| 120x40-r1-Escape-staged | Independent Escape-run staged draft |
| 120x40-r1-Escape-after-Escape | Draft remains; Esc again to clear |
| 120x40-r1-Escape-second-Escape-cleared | Second Escape clears the draft |

Only private paths, including clipped path fragments, become equal-length
`/tmp/xxx…` placeholders. Rows, spaces, ANSI sequences and editor text stay
unchanged; tests remove only the added provenance line. Sequential plain and
styled observations are not an atomic pair. No Enter was sent in this probe.

`tests/staged_reference.rs` checks both reference lengths at wide widths,
wrong references, plain-input refusal, cleared negatives and explicitly
Synthetic changed-token/outside-editor mutations. All 28-column exact reads
currently return None, including the partial Ctrl-U draft. One Escape does
not clear the reference: its wide frame still matches and must not be called
a cleared negative. Wide control facts read Draft / NotRunning / None, with
control dialog None. Matching alone is not authorization to submit.

## Staged clear command (P5)

Captured: Claude Code 2.1.325, 2026-11-14, P5 sealed loopback-only private
tmux 3.8 `capture-pane -p -e -N` (styled) and `-p -N` (plain).
Each row maps `a-initial/A-claude-<size>-<state>.{styled-N,plain-N}.txt`
to `screen_p5_<size>-<state>_2.1.325.{styled,plain}.txt`.

| Size | State |
|---|---|
| 80x40 | staged |
| 100x40 | staged |
| 120x40 | staged |

Only private run roots become equal-width generic `/tmp/xxx…` paths. All
terminal rows, ANSI sequences and spaces remain; tests strip only the added
provenance line. The selected `/clear` completion has a wrapping description
at narrower widths. Styled staged matching is exact and control is None;
delivery remains Unknown. Plain retained-space frames remain Unknown and
cannot prove a staged draft. `tests/staged_clear.rs` pins these facts and
labels editor/text mutations Synthetic. A match alone never authorizes Enter.

## Lifecycle control observations (B0)

Captured: Claude Code 2.1.325, 2026-11-14, sealed P1 private-tmux
`capture-pane -p -e -N`. `screen_control_p1_<stem>_2.1.325.txt` maps to
`<stem>.styled-N.txt`; corresponding `.plain.txt` and `.meta.json` contain
wording and cursor/geometry evidence. Only private run roots are replaced by
equal-width `/tmp/xxx…` paths. Attributes, rows, spaces and composer text are
unchanged; the first provenance line is removed at test load.

`tests/control_screen.rs` covers A80/A120/A200 default, selected, reset and
ready frames; A-race-0/1-yes; A-auto-ready; B-clear-007;
B-baseline-conversation; C1-auto-settled; B-auto-submitted; and C3-dialog.
The new control observer recognizes both trust selections and captured manual/
auto launch chrome. Lines above the first complete header are uninterpreted;
a later copied header cannot erase earlier history. The legacy observer and
its existing Unknown assertions are unchanged. Mutations are marked Synthetic.

Full trust paths are not filesystem authorization; long/wrapped paths and
unseeded onboarding remain unverified. Retry count is policy, not a key sender.
Conversation=false is not proof of freshness. The four-option permission
capture remains Unknown. Older limitations below describe the legacy observer.

## Styled fixtures (U8d)

Captured: Claude Code 2.1.325, 2026-11-14, U5b private tmux 100x30
`capture-pane -p -e`, and P1 private tmux `capture-pane -p -e -N`.
`screen_styled_u5b_<state>_2.1.325.txt` maps to
`claude-<state>.sgr.txt`; `screen_styled_p1_<state>_2.1.325.txt` maps to
`<state>.styled-N.txt` (dimensions are in each P1 state name). Headers are
provenance, removed only at test load. U5b is attribute evidence, not retained
space evidence. Its cursor sidecars are `<source without .sgr.txt>.cursor.txt`;
P1 geometry/cursor sidecars are `<state>.meta.json`.

U5b bytes are unchanged. P1 replaces only the non-composer private run root
with an equal-width `/tmp/xxx…` token; no row, SGR, OSC, space or glyph moves.
The retained tests cover three terminal modes, Home drafts, blank editors,
80/120/200-column trust, Yes-selected refusal, manual mode, four-option
permission, and a shell launch prefix. Synthetic mutations are labelled in
`tests/styled_screen.rs`.

These styled tests supersede the **plain-only** limitations below: inverse/faint
suggestions are distinguishable, while coordinates alone still prove nothing.
Blank/padding stays Empty despite its whitespace-only-draft collision; consumers
must verify the exact staged draft before Enter. Leading whitespace is preserved.
Multiline reconstruction returns unknown. Trailing ASCII spaces are chrome;
expected text must have no trailing whitespace. `-N` retains allocated grid
padding, not just authored spaces (verified with tmux 3.8 and 3.6).
Captured: Claude Code 2.1.325, 2026-11-14, sealed private-tmux command probe:
`/clear   ` followed by Enter emitted SessionEnd/SessionStart with reason/source
clear, not UserPromptSubmit; no API request occurred.

P1 `B-auto-pasted-120x40` pins a matching padded real draft. `B-clear-001-120x40`
pins the slash menu (it replaces the footer), and
`C2-queued-001-120x40` pins the unsupported manual-mode queued frame. Legacy
facts remain Unknown for both. B0's staged matcher now checks the single selected
completion command and active editor exactly; description wording may vary but
wrapped description alignment must remain intact. No selection, two selections,
missing editor or malformed layout refuses. The menu is None (not a modal) only
in the control observer. The manual queued staged check remains None. The queued
frame's captured editor row is also transplanted into supported auto chrome in
a labelled Synthetic test. A match alone never proves what Enter will do.

The original text fixtures are the 15 distinct frames among 48 raw U5 Claude
screens (40 rows, 120 columns). The additional coordinator and U5b frames are
described below; there are 65 Claude screen fixture files in total. Existing JSON/JSONL hook fixtures retain the
provenance in the crate docs. `screen.rs::tests::captured_screens` asserts all
four facts for every frame; no `.prompt.txt` presentation excerpts are used.

Every row below has this provenance:
**Captured: Claude Code 2.1.325, 2026-11-14, U5 private-tmux capture; sanitized.**

A source `claude-<name>.txt` maps to
`screen_claude-<name>_2.1.325.txt`. Duplicate source names listed below were
byte-identical before sanitization; one fixture covers each group.

| Source representative | Identical source names (without `.txt`) | Observed meaning |
|---|---|---|
| `claude-startup.txt` | — | Default-No folder trust, full folder path |
| `claude-permission.txt` | — | Bash permission dialog |
| `claude-denied-0.txt` | — | Permission dialog still visible immediately after Escape |
| `claude-before-escape.txt` | `claude-escape-0` | Empty input, turn running |
| `claude-clear-poll-01.txt` | — | Empty input, SessionEnd hook running |
| `claude-escape-5.txt` | `claude-escape-05`, `claude-escape-2` | Restored draft, not running |
| `claude-clear-poll-02.txt` | `claude-clear-poll-03` through `claude-clear-poll-29` | Empty fresh editor, `/clear` retained in history |
| `claude-arrival.txt` | `claude-clear-poll-00` | Empty editor after a reply; not fresh |
| `claude-after-clear.txt` | — | Already contains the next prompt/reply; not fresh |
| `claude-denied-5.txt` | `claude-denied-05`, `claude-denied-2` | Empty editor after denial; not fresh |
| `claude-gate.txt` | — | Fresh layout, ambiguous suggested input |
| `claude-ready.txt` | — | Fresh layout, ambiguous suggested input |
| `claude-restarted.txt` | — | Fresh layout, different ambiguous suggestion |
| `claude-shell.txt` | — | Shell, no recognized active harness facts |
| `claude-final-shell.txt` | — | Shell after restart/exit, no recognized facts |

## Sanitization

Only these substitutions were made, verified against a source-to-fixture diff:

- The private run root became `/tmp/u8a-fixture`; this also rewrites cwd and
  socket paths. Home prefixes, if present, become `/home/user` (none occurred
  in these 15 frames).
- The capture helper path became `/tmp/u8a-fixture/bin/capture.py`.
- The run's agent tag became `fixture/claude`.
- The resume UUID became `00000000-0000-4000-8000-000000000001`, consistently.
- Shortened lines were right-padded with ASCII spaces to preserve their
  original character count. These intentional trailing spaces are capture
  geometry, not draft content. No existing line was rewrapped or removed.

Rows, borders, NBSP, input content, blank lines and footer columns are
otherwise unchanged. Assertions use only sanitized paths. No real IDs,
account identifiers, home names or local infrastructure paths are retained.

## Missing states and limits

- The U5 fixtures have no attributes or cursor coordinates. U5b captured both
  separately, but only its plain `capture-pane -p` bodies are consumed here;
  tests always pass `None`. Styled parsing is a separate follow-up.
- Suggested startup input and an identical typed draft cannot be distinguished
  by this evidence. Input stays Unknown, even when fresh layout is Present:
  **no delivery into a freshly started Claude editor under this contract.**
- Supplied cursor metadata currently refuses observation (all Unknown); fake
  coordinates in tests are explicitly synthetic refusal tests, not captures.
- Other versions, themes, permission modes, alternate trust selection,
  other dialogs, multiline/whitespace-only drafts
  and incomplete/scrollback layouts have no positive real coverage here. U5b
  adds real free-text drafts and 100-column editors; U5 has the restored Escape
  draft. Adversarial draft/path/control alterations are marked Synthetic.
- Fixed borders, footer/help/dialog wording and prompt glyphs are exact.
  Versions, model names, effort levels, spinner frames and the turn verb are
  values, not gates. A spinner plus an alphabetic word ending in `…` is active,
  with optional parenthesized details. Tips and effort/update notices may be
  absent or vary. Unrecognized status structure is Unknown, never inferred idle.
- Clear of an already-fresh editor, clear in a Git worktree, and timing relative
  to a particular clear command were not verified. Present is layout evidence,
  not a new generation, session identity or permission to paste.
- Recognition cannot distinguish a live screen from an identical historical
  copy. Consumers must establish pane/process identity, current viewport,
  copy mode, freshness and stable safe observations separately.

## Additional coordinator capture

**Captured: Claude Code 2.1.325, 2026-11-14, coordinator read-only tmux capture-pane -p; sanitized.**

`claude-2.1.325-running-bottom25.txt` maps to
`screen_claude-extra-running-bottom_2.1.325.txt`. It is a cropped bottom-only
viewport with `✻ Generating…`, parenthesized elapsed/token details, a tip,
max effort/update notice, and background-agent rows below the composer.
**The background-agent footer is deliberately unsupported: the real frame
returns all Unknown.** It is not an ordinary standard footer, and no safe
selection/action semantics have been established for it.

A labelled synthetic variant replaces only that unsupported footer with the
previously captured standard footer: the complete bottom layout then reports
Empty/Running/None, without requiring the scrolled-off startup header. This
checks the new verb/status shape without misrepresenting background UI support.
Fresh-layout evidence still needs a recognizable startup/cleared history.

Additional sanitization replaces all nonblank transcript rows above the active
spinner with padded redaction markers. The background task description and
personal free-review quota are redacted in place. Every row and character count,
blank row, border, input glyph, spinner, effort label and status column remains;
no row is rewrapped. Local paths, identifiers and conversation text are removed.
The original 15 U5 fixtures and all their expected facts are unchanged.

## U5b inline editor layouts

**Captured: Claude Code 2.1.325, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.**

All 49 Claude `.plain.txt` states are copied one-to-one, including duplicates:
`claude-<state>.plain.txt` → `screen_u5b_claude-<state>_2.1.325.txt`.
Each file retains its own original `Captured:` first line (including terminal
mode); tests remove that label, not any terminal rows. The evidence producer
already replaced temporary home/config/run paths with `~`. Copies are
byte-identical to those sanitized sources: no further substitutions, padding,
normalization or rewrapping. No `.sgr.txt` or `.cursor.txt` is parser input.

`screen.rs::tests::u5b_captured_screens` lists all 49 names with all four expected
facts. Each family covers fresh, same-pattern/text draft, different draft,
erased, after-clear and post-clear drafts; additional names cover Ctrl+A,
settled post-clear and a repeated fresh start. `screen-with-colorterm` is the
misconfigured first poor-terminal trial, not evidence of absent COLORTERM.

| Observed plain body | Input | Turn | Dialog | Fresh layout |
|---|---|---|---|---|
| Blank composer, empty or only `/clear` history | Empty | NotRunning | None | Present |
| `Try "…"` suggestion **or identical draft**, otherwise fresh | Unknown | NotRunning | None | Present |
| `baseline draft 123` | Draft | NotRunning | None | Unknown |
| Separate update/focus-warning footer rows (four names below) | Unknown | Unknown | Unknown | Unknown |

The one-time auto-mode information banner in the `rich` family is recognized
immediately after the header by its complete fixed wording and URL, joining
wrapped paragraph rows. It is not conversation. A damaged/unknown banner is
not skipped. Fresh-layout evidence never proves input emptiness or completion
of a particular clear.

The four refused frames are `rich-enabled-same-text-draft`,
`screen-with-colorterm-same-pattern-draft`,
`screen-with-colorterm-enabled-same-text-draft`, and
`screen-with-colorterm-enabled-same-text-draft-cursor-start`. These have an
additional update row and truncated tmux focus warning below the normal
footer, rather than a right-aligned notice on that footer. Their bottom
structure remains unrecognized; no fact is fabricated from these drafts.

The header may start at any first nonblank row and cwd may be exactly `~`.
The input can follow content directly or sit at the pane bottom; a done line
only two rows above the border is idle evidence. The left footer chrome stays
exact, with optional right-aligned notices separated by at least two spaces
and the known `Ctrl+Y to paste deleted text` hint below. Unknown footer rows
still refuse. Synthetic tests cover each independent layout change, shifted
spinners, 80/100/120/200-column borders, and damaged notices/footer chrome.
All tests are pure, with no waits or terminal operations.

## Review regressions and remaining limits

- Trust paragraph matching joins wrapped lines between the single absolute
  folder line and fixed capability/options chrome. Synthetic rewraps of the
  real capture at 80, 100, 120 and 200 columns preserve FolderTrust; modified
  wording, wrapped/truncated paths and changed selection still refuse.
- Linux's `*` spinner frame is recognized along with the prior frame set.
- A `/clear` echo below old conversation gives turn Unknown, not idle. Only
  an otherwise empty recognized fresh history may retain that echo as idle.
- **F2:** `capture-pane -p` drops trailing spaces. A spaces-only draft can
  therefore read Empty; ASCII-space synthetic tests exercise preservation of
  supplied text, not protection in the real capture pipeline. Styled capture
  with `-N` is deferred to U8d; no plain-text fix is claimed here.
- **F9:** permission recognition still requires the captured Bash auto-mode
  ask-rule wording. Editor footer modes other than auto (accept-edits, plan,
  bypass-permissions, shortcut-only, background shell/agents) remain Unknown.
  Configuration-specific dialogs need more real captures before generalizing.

The unit adds no RPC method. These capture tables and synthetic refusals are
parser conformance coverage; launch/clear constants have exact policy tests.
