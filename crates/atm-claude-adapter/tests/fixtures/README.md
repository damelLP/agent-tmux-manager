# Claude screen fixtures

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
- Other versions, themes, permission modes, terminal widths with wrapped dialog
  UI, alternate trust selection, other dialogs, multiline/whitespace-only drafts
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

The unit adds no RPC method. These capture tables and synthetic refusals are
parser conformance coverage; launch/clear constants have exact policy tests.
