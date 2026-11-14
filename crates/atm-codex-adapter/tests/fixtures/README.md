# Codex screen fixtures

## Finished-turn clocks (U8g)

Captured: codex-cli 0.167.0, 2026-11-14, U5 and sealed P1 private-tmux
captures. P1 contains `<1s`, `1s`, `7s` and `16s` finished durations with
24-hour `HH:mm` clocks. U5 supplies `1s • 11:31 AM` and `2s • 11:32 AM`,
the 12-hour clock evidence; P2 has no Worked lines. The sealed long-turn
captures below add `1m 5s`.
Finished durations accept `<1s`, seconds 1–59, minutes 1–59 plus seconds 0–59,
or positive hours plus minutes/seconds 0–59, all without zero padding.
Hour-duration shapes follow the codex-cli 0.167.0 binary's formatter and are
Synthetic test coverage, **not verified on screen**. Running-duration parsing
is unchanged. The 12-hour hour is unpadded (1–12); the 24-hour hour is padded
(00–23), and minutes are padded (00–59). Extra text or malformed separators
are not completion evidence. Date-prefixed clocks (for example `• Nov 13 at
16:34`) stay Unknown; a resumed session may remain Unknown until its next turn.

The existing P1 c02/c03 and B0 b12 fixtures now read NotRunning, retaining their
other facts. New `screen_styled_p1_<stem>_0.167.0.txt` fixtures map to P1
`<stem>.styled.txt`, captured with `capture-pane -p -e -N`:

- `c11-working-released-100x24`: final `7s` completion.
- `c18-permission-enter-approved-100x32`: final `16s` completion.
- `c21-permission-multi-approved-100x32`: final `1s` completion.

Only the private run root is replaced with an equal-width `/tmp/xxx…` token;
attributes, whitespace and rows remain intact. `tests/worked_status.rs` tests
these full frames, c02/c03, old Worked lines before working/dialog suffixes,
and explicitly Synthetic clock-boundary and malformed-line mutations.

Captured: codex-cli 0.167.0, 2026-11-14, private network namespace with the
unchanged P1 loopback Responses stub held for 65 seconds, private tmux 3.8
`capture-pane -p -e -N`, at 100x32 and 120x40. Both
`screen_styled_u8g_finished-<size>_0.167.0.txt` frames show
`Worked for 1m 5s • 21:19`. Only the private run root is replaced with an
equal-width `/tmp/xxx…` token; styles and rows are unchanged. Launched as
`codex --no-daemon --no-alt-screen -C <dir> '<prompt>'`, with no pane keys
sent or real API access. P3 isolated `--no-alt-screen` as the compact-footer
trigger, not the positional prompt or `-C`. P3's spawn-today probe
(`codex '<prompt>'`) and separate agent-policy probe
(`codex --no-daemon '<prompt>'`) both have the normal footer. Today's atm
launch applies that agent policy, so prompt-started Codex still has the
normal footer. The compact empty footer lacks the supported shortcuts row:
the full observer still refuses it. A unit test reads its real duration line
directly. No compact-footer support or style-based completion recognition is
added here.

Captured: codex-cli 0.167.0, 2026-11-14, P3 sealed loopback-only Responses
stub held 65 seconds, private tmux 3.8 `capture-pane -p -e -N`. Launched as
`codex --no-daemon`, with a typed prompt. The normal-footer fixtures
`screen_styled_p3_<stem>_0.167.0.txt` map to `a-matrix/<stem>.styled-N.txt`:

- `A-p1-typed-100x32-idle`: `Worked for 1m 5s • 22:08` at 100x32.
- `A-p1-typed-100x32-minute-idle-resized-120x40`: the same completed turn
  after resize to 120x40.
- `A-p1-typed-100x32-held-over-minute`: Working followed by a trailing tip.

Only the private run root becomes an equal-width `/tmp/xxx…` token; styles,
rows and spaces are unchanged. Tests strip only the provenance line. The
minute pair replaces the Synthetic completion-line transplant: both public
observers read Empty / NotRunning / no dialog / Unknown freshness, and the
control observer reads conversation true.

**Known limit:** the held-over-minute frame shows `Working (1m 05s • esc to
interrupt)` followed by `└ Tip: Use /rename to rename your threads for easier
thread resuming.`. Only the last nonblank history row is checked for Working,
so both observers read Empty / Unknown turn / no dialog / Unknown freshness,
with conversation true. This is a trailing-tip limit, not a duration failure;
the test pins today's fail-closed reading, not support for this running state.

## Lifecycle control observations (B0)

Captured: codex-cli 0.167.0, 2026-11-14, sealed P1 private-tmux
`capture-pane -p -e -N`. `screen_control_p1_<stem>_0.167.0.txt` maps to
`<stem>.styled.txt` (retained spaces despite its filename); `.plain.txt` and
`.meta.json` accompany the original captures. Private run roots alone become
equal-width `/tmp/xxx…` paths; attributes, rows, spaces and composers remain.
Tests remove the provenance label, not terminal rows.

Control tests cover a02/a10/a17 default Review hooks; a14/a16 Folder access;
b27/b31/b35/b38 current-worktree selection; a04 pending and a05/a06 trusted
hook pages; b12 conversation, plus existing styled working/fresh fixtures.
The update prompt uses the existing U5 startup/restart-startup Plain captures:
U5's control-plane findings verified Escape skips that launch; P1's sealed
probe could not provoke an update prompt. No styled update evidence is claimed.
Warnings is Blocked, not LaunchSkip; unsupported modals stay Unknown.

All three P1 hook pages show eight required event rows with a down indicator.
B0's complete-set tests used Synthetic data; P2 below now supplies real lower
pages. P6 remains unchanged: exactly the eleven ATM names, Installed=1 each;
pending Active in {0,1}, Review=1-Active; trusted Active=1, no Review column.
Missing/duplicate/extra complete-set names and mixed layouts refuse.
Counts/event names cannot establish command identity: a foreign replacement
with identical event/count is undetectable. Page traversal and sending literal
t are not implemented here. Descriptions validate grammar, not identity.
B0 left Worked-line parsing unchanged; U8g's finished-turn correction is
documented above. The older limitations below describe the legacy observer.

## Complete hook-list evidence (P2)

Captured: codex-cli 0.167.0, 2026-11-14, sealed P2 private tmux 3.8,
`capture-pane -p -e -N`. All 263 list frames are retained as
`screen_control_p2_<stem>_0.167.0.txt`, mapping to source
`p2-<stem>.styled.txt`. The versioned TSV manifest lists every fixture and its
first visible event, independently extracted from companion plain captures.
Only the private run root becomes an equal-width `/tmp/xxx…` path; terminal
attributes, rows, spacing, counts, descriptions and selections are unchanged.
Tests strip only the added provenance line. No harness is launched by tests.

The manifest includes the full pending/trusted down/up sweeps, both wraps and
restored top, plus the initial exploratory pending sweep. Scenarios: eleven
registered hooks at 100x32, 100x24 and 120x40; ten registered hooks (missing
SubagentStop) at 100x32. Every viewport has eight rows, selected at any position;
scrolling moves one row at a time. The complete UI vocabulary has fourteen
rows, not eleven. Top pages have only a lower marker, middle pages both, and
bottom pages only an upper marker. Down at the bottom wraps to the top, not a
no-op; consumers must not wait for a no-op to establish completeness.

The parser validates contiguous UI order, exact edge markers and exactly one
selection. Expected descriptions are clipped at the widest captured line minus
the header's Description column (counted in characters), then compared after
trimming trailing spaces. Pending width-100 frames clip Interrupt and
PostModelRequest. The selected row spans the pane; header padding is tmux
allocation, not a clipping edge. Replays of real Codex bytes in tmux 3.8 show
half-width header allocation from 142 columns and quarter-width from 284.
A live Codex at 142 columns or wider and layouts below 100 columns are
unverified. A combining mark on a full-width line widens the inferred edge
by one character: only pending 100-column pages with clipped descriptions
refuse in the captured corpus (a fail-closed limit). Trimmed captures and
pages without clipped descriptions still parse.
Zero-count rows parse even for missing ATM registrations. Only the known
uninstalled Interrupt, PreModelRequest and PostModelRequest rows are omitted
from returned authorization entries: unknown events or installed extras refuse
the entire page. Zero-count SubagentStop is retained, so the ten-hook collected
sets fail P6. Returned rows never imply that a page is complete; `above`/`below`
describe the full UI vocabulary.

Tests replay all 263 frames, compare exact entries/counts and edge flags, and
collect eight sets with consistent overlaps and evidence of both ends. Six
complete eleven-hook sets pass P6; both ten-hook sets and every proper subset
of every set refuse. Marked Synthetic mutations of every real page cover
missing/damaged markers, false ends, missing/duplicate/reordered rows,
missing/multiple selections, damaged chrome/descriptions, extra installations
and unknown events. Conflicting repeated counts and mixed layouts are refused
by complete-set validation; a stateless observer cannot compare separate
frames, so consumers must compare overlaps before deduplicating them.

No new controller/collection API is added. Other versions, sizes, plugin hook
combinations and same-count command identity remain unverified.

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
The staged matcher never removes leading whitespace. It refuses multiline
reconstruction and treats trailing ASCII spaces as chrome; expected text must
have no trailing whitespace. P1 `c03`, `b05`, `b09` and `b23` pin all four real
padded drafts. A match is not permission to press Enter: `/`, `@` and `$`
completion popups above the composer must be handled by the consumer.
Captured: codex-cli 0.167.0, 2026-11-14, sealed private-tmux command probe:
`/new   ` followed by Enter replaced startup help with a fresh logo/editor;
no API request occurred.
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
