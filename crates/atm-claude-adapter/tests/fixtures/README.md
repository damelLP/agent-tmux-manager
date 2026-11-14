# Claude screen fixtures

These text fixtures are the 15 distinct frames among 48 raw U5 Claude
screens (40 rows, 120 columns). Existing JSON/JSONL hook fixtures retain the
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

- No attributes or cursor coordinates were captured. These are plain
  `capture-pane -p` frames; tests always pass `None` for their cursor.
- Suggested startup input and an identical typed draft cannot be distinguished
  by this evidence. Input stays Unknown, even when fresh layout is Present:
  **no delivery into a freshly started Claude editor under this contract.**
- Supplied cursor metadata currently refuses observation (all Unknown); fake
  coordinates in tests are explicitly synthetic refusal tests, not captures.
- Other versions, themes, permission modes, terminal widths with wrapped UI,
  alternate trust selection, other dialogs, free-text/multiline/whitespace-only
  drafts and incomplete/scrollback layouts have no positive real coverage here.
  The restored Escape draft is the real positive draft fixture. Alterations for
  adversarial draft/path/control tests are marked Synthetic in their tests.
- Only the captured working status layouts are recognized. An unknown status
  in the active status region yields Unknown turn state, never inferred idle.
- Clear of an already-fresh editor, clear in a Git worktree, and timing relative
  to a particular clear command were not verified. Present is layout evidence,
  not a new generation, session identity or permission to paste.
- Recognition cannot distinguish a live screen from an identical historical
  copy. Consumers must establish pane/process identity, current viewport,
  copy mode, freshness and stable safe observations separately.

The unit adds no RPC method. These capture tables and synthetic refusals are
parser conformance coverage; launch/clear constants have exact policy tests.
