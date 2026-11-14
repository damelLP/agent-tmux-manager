# Shared stand-in harness (test only)

Python standard library fixture for root integration tests. Never invokes a
model CLI or executes submitted text. Linux only (procfs process identity).

## Captured templates

Captured: Claude Code 2.1.325, 2026-11-14, U5 real TUI via timestamped private
Unix proxy into atmd. `claude_2.1.325.jsonl` selects source envelope lines
1, 3, 4, 6, 7, 14, 15, 18, respectively. The existing adapter's
`claude_tui_session_2.1.310.jsonl` independently confirms these lifecycle shapes.

Captured: Codex 0.167.0, 2026-11-14, U5 real TUI with `--no-daemon` via the
same proxy. `codex_0.167.0.jsonl` selects lines 3, 4, 5, 12.

Captured: pi 1.5.0, 2026-11-14, U5 real TUI with pi-atm via the same proxy.
`pi_1.5.0.jsonl` selects lines 1, 2, 3, 5, 6, 7, 8, 18, 19. The agent_end
messages are excerpts retaining only the assistant; system/tools/user context
and signed text metadata are omitted. This mode **replays captured envelopes;
it does not run pi or pi-atm**.

Sanitization: original paths become `/fixture/...`; session/prompt/turn/tool/
response identifiers become `fixture-*`; timestamps become zero. Forwarder-
added PID and pane fields are removed: runtime must derive them. Model names,
reason/source/stopReason fields and usage shapes are retained. The permission
screen is an excerpt of U5's Claude permission capture with marker text replaced.

Synthetic: runtime prompt, ID, path and timing substitutions construct controlled
scenarios from these templates. They are not new real-harness captures. No native
Codex or pi dialog was captured by U5, so neither mode claims dialog fidelity.
