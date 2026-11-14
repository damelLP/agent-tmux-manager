# Codex screen fixtures

## Styled fixtures (U8d)

Captured: codex-cli 0.167.0, 2026-11-14, U5b private tmux 100x30
`capture-pane -p -e`, and P1 private tmux `capture-pane -p -e -N`.
`screen_styled_u5b_<state>_0.167.0.txt` maps to
`codex-<state>.sgr.txt`; `screen_styled_p1_<state>_0.167.0.txt` maps to
`<state>.styled.txt` (P1 used -N despite the filename; dimensions are in the
state name). Provenance headers are removed only at test load. U5b cursor
sidecars are `codex-<state>.cursor.txt`, P1 sidecars `<state>.meta.json`.

U5b bytes are unchanged; P1 replaces only private run roots outside composers
with equal-width `/tmp/xxx…` tokens. All SGR, row/space geometry and visible
composer text remain. Tests cover three terminal modes, 24/30/32 rows, logo
and no-logo startup, Home drafts, working, submitted multiline history,
slash completion and folder/hook/worktree/permission refusals.

Styled evidence supersedes the plain-only limitations below: an all-faint
placeholder in a complete regular editor reads Empty; typed text reads Draft.
`-N` preserves allocated padding, not just authored spaces (tmux 3.8/3.6 probes).
The exact staged matcher never removes leading whitespace. It refuses multiline
reconstruction and trailing-padding ambiguity rather than authorizing Enter.
New mutations are explicitly Synthetic in `tests/styled_screen.rs`.

Fourteen distinct frames cover all 19 raw U5 Codex screens (40 rows, 120
columns). Existing JSONL fixtures retain their crate-doc provenance. The
screen parser's `captured_screens` table asserts all four facts for each frame.

Every row below has this provenance:
**Captured: Codex 0.167.0, 2026-11-14, U5 private-tmux capture; sanitized.**

Source `codex-<name>.txt` maps to `screen_codex-<name>_0.167.0.txt`.
Duplicates were byte-identical before sanitization; `.prompt.txt` excerpts
are not used.

| Source representative | Identical source names (without `.txt`) | Observed meaning |
|---|---|---|
| `codex-startup.txt` | — | Interactive update menu: Other dialog |
| `codex-restart-startup.txt` | — | Same menu with newer offered version: Other |
| `codex-warning.txt` | — | Startup warning viewer: Other |
| `codex-post-update.txt` | `codex-restarted` | Idle editor, noninteractive update banner, no dialog |
| `codex-status.txt` | — | Idle editor after status output, no dialog |
| `codex-arrival.txt` | — | Idle editor after reply, no dialog |
| `codex-after-clear.txt` | — | Already contains next reply, not fresh |
| `codex-permission.txt` | — | Idle model refusal, NOT a permission dialog |
| `codex-escape-5.txt` | `codex-escape-05`, `codex-escape-2` | Idle interrupted conversation |
| `codex-before-escape.txt` | `codex-escape-0` | Running turn, ambiguous placeholder |
| `codex-clear-poll-00.txt` | — | `/new` draft plus completion menu (Other), idle |
| `codex-clear-poll-01.txt` | `codex-new-picker` | Fresh logo layout, ambiguous placeholder |
| `codex-shell.txt` | — | Shell: all Unknown |
| `codex-final-shell.txt` | — | Shell after another exit: all Unknown |

All regular composers display `Ask Codex to do anything`. Plain text cannot
distinguish this placeholder from an identical draft: input is Unknown,
**never Empty; no delivery into Codex under this contract yet**. `/new` with
its completion menu is the sole real positive Draft fixture.

## Sanitization

The reviewed source-to-fixture diff contains only:

- Private run root -> `/tmp/u8a-fixture`, including cwd/socket paths.
- Capture helper -> `/tmp/u8a-fixture/bin/capture.py`.
- Home prefix -> `/home/user`; agent run tag -> `fixture/codex`.
- Prompt run markers -> `fixture1` / `fixture2`, same character count.
- All session UUIDs -> `00000000-0000-4000-8000-000000000001` (identity
  relationships are not observations this parser makes).
- Already-redacted account/workspace identifiers kept; account plan replaced
  by `(account)`, weekly quota/reset details and credits by redaction labels.
- Shortened lines right-padded to their original character count. These
  intentional trailing spaces are capture geometry, not composer content.

No source row was removed, added or wrapped. Glyphs, blank rows, logo,
composer and footer alignment remain intact. No original home/run paths,
real UUIDs or personal quota values remain. Status parsing does not depend
on the substituted quota/account/session values.

## Missing states and limits

- No real permission/trust dialog was captured. Those shapes remain Unknown;
  the misleading permission filename cannot supply positive approval evidence.
- U5b adds real free-text drafts, which remain all Unknown because they lack
  the regular shortcut footer. Multiline/whitespace drafts remain synthetic.
- U5 has no cursor/attributes; U5b has sidecars, deferred to U8d. Tests pass
  None; supplied cursor metadata refuses observation even at the prompt's Home
  position. Cursor/control/truncation mutations are explicitly Synthetic.
- Fixed footer/dialog labels, separators, option wording and prompt glyphs
  remain exact. Versions, model/effort, cwd/branch/title, elapsed times, clock
  values and warning counts match their shapes, not captured literals. A
  scrolled-off header does not invalidate a complete ordinary bottom layout;
  fresh evidence still requires the header. Changed structure (unknown
  UI, damaged border/footer, unsupported dialog) remains Unknown. A missing
  shortcut footer is not a regular editor; only the captured completion menu
  has that variant.
- The original value generalization used only U5; U5b now extends real coverage.
  `value_variations_preserve_facts` changes values in each named U5 fixture,
  explicitly labelled Synthetic; malformed-value and structural-damage tests
  retain conservative refusals. All original real fixture assertions remain.
- The status capture has a clipped old update banner but a complete current
  header/status/composer/footer. Truncated current UI is not an idle signal.
- Fresh evidence requires the header, regular composer/footer and no conversation;
  getting-started help and the complete logo are optional. Neither a partial
  logo nor arbitrary content is fresh.
  `new-picker` and `clear-poll-01` are identical but came from different
  operations: Present is not proof a particular clear completed or a new
  generation began. Already-fresh/Git-worktree clears are unverified.
- A historical full copy cannot be distinguished from live text. Consumers
  own current viewport, pane/process identity, copy-mode and temporal checks.
- Launch policy exports `/new`, required `--no-daemon`, model flag `-m`.
  Legacy registry/spawn behavior is unchanged; daemon/role argv wiring is
  later work, not exercised here.

## U5b captures and anchor regressions

**Captured: codex-cli 0.167.0, 2026-11-14, U5b private tmux 100x30 capture-pane -p; sanitized.**

All 24 `codex-<state>.plain.txt` sources map one-to-one to
`screen_u5b_codex-<state>_0.167.0.txt`. Files retain their individual provenance
first line; tests remove only that label. The producer already replaced paths
with `~`; copies are byte-identical, including blank rows. Each of rich, poor,
and the supplementary screen-with-colorterm modes has eight states:

| States in each mode | Input / Turn / Dialog / Fresh |
|---|---|
| fresh, erased, after-clear | Unknown / NotRunning / None / Present |
| same-text-draft, same-text-draft-cursor-start, different-draft, after-clear-same-text-draft, after-clear-different-draft | Unknown / Unknown / Unknown / Unknown |

`u5b_captured_screens` asserts all four facts for all 24 names. Fresh/erased
have startup help; after-clear has neither help nor logo. All use cwd `~`.
The placeholder remains Unknown in plain text. Synthetic vertical relocations
check that a first-nonblank header and trailing Working status use their chrome,
not row numbers. F12's renamed fresh-layout test checks determinism and rejection
of conversation/partial logos, not generation tracking.

**F9 known limit:** update dialogs still require the captured first selection
and npm-install wording; the warning viewer requires a one-line message.
Other configurations safely refuse as Unknown, pending real captures.

No RPC method is added. Captures and synthetic refusals are parser conformance
coverage; static policies have exact-value tests.
