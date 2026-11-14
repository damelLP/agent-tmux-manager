# Codex screen fixtures

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
- No free-text/multiline/whitespace draft was captured. Synthetic mutations
  test refusal, not claims about real UI rendering.
- Captures contain no cursor or attributes (`capture-pane -p`). Tests pass
  None; supplied cursor metadata refuses observation even at the prompt's Home
  position. Cursor/control/truncation mutations are explicitly Synthetic.
- Only captured version/model/effort/footer/title, elapsed status values and
  update/warning selections are supported. Other times, models, titles,
  versions, themes, wrapping, statuses and dialogs fail conservatively until
  evidence-backed support is added. A missing shortcut footer is not accepted
  as a regular editor; only the captured completion menu has that variant.
- The status capture has a clipped old update banner but a complete current
  header/status/composer/footer. Truncated current UI is not an idle signal.
- Fresh evidence requires the complete captured logo without conversation.
  `new-picker` and `clear-poll-01` are identical but came from different
  operations: Present is not proof a particular clear completed or a new
  generation began. Already-fresh/Git-worktree clears are unverified.
- A historical full copy cannot be distinguished from live text. Consumers
  own current viewport, pane/process identity, copy-mode and temporal checks.
- Launch policy exports `/new`, required `--no-daemon`, model flag `-m`.
  Legacy registry/spawn behavior is unchanged; daemon/role argv wiring is
  later work, not exercised here.

No RPC method is added. Captures and synthetic refusals are parser conformance
coverage; static policies have exact-value tests.
