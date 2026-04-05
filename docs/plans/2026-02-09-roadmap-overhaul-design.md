# Roadmap & Progress Tracking Overhaul

**Date:** 2026-02-09
**Status:** Approved — ready to execute

## Problem

6+ planning/tracking files totaling ~9,500 lines with significant overlap and staleness:
- `ROADMAP.md` (1,594 lines) — still says "Ready for Implementation", never updated
- `PROGRESS_TRACKER.md` (886 lines) — last updated Jan 25, says 270 tests (actual: ~416)
- `atm-plan/WEEK_1*.md` (1,295 lines) — historical, completed
- `atm-plan/WEEK_2-3*.md` (2,518 lines) — historical, completed
- `atm-plan/WEEK_4*.md` (2,373 lines) — historical, completed
- `atm-plan/WEEK_5+*.md` (783 lines) — partially outdated

No single source of truth for "where are we, what's next."

## Solution

### 1. Single compact ROADMAP.md (~80 lines)

Replaces both `ROADMAP.md` and `PROGRESS_TRACKER.md`. Structure:

```
# Agent Tmux Manager — Roadmap

## Status: v0.1.3 — MVP Complete, In Daily Use

## What's Shipped
[Summary table: phase | summary | test count]
**416 tests passing** | **Released: v0.1.3**

## What's Next

### Near-term
- [ ] Projects — group sessions by CWD (#issue)
- [ ] Teams/Subagents — parent-child relationships (#issue)
- [ ] Filtering — by status, agent type, search (#issue)
- [ ] Preview pane — richer detail view (#issue)
- [ ] Vim navigation — gg, G, etc. (#issue)
- [ ] Session actions — kill with confirmation (#issue)
- [ ] Pre-select current pane — auto-highlight last session (#issue)

### Later
- [ ] Help screen (? key)
- [ ] Config file (~/.config/atm/config.toml)
- [ ] User documentation & README polish
- [ ] Binary packaging / release automation

## Architecture
[2-3 sentence summary pointing to docs/ for details]
```

### 2. GitHub Issues for detail

Each near-term item becomes a GitHub Issue with:
- Summary (1-2 sentences)
- Design Notes (open questions, approach options, relevant code links)
- Acceptance Criteria (concrete checkboxes)

Labels: `feature`, `near-term`, `later`

No GitHub Project board yet — ROADMAP.md "What's Next" section is the board.
Issues are where the detail lives.

### 3. File changes

| Action | Files |
|--------|-------|
| **Delete** | `atm-plan/` (entire directory — 4 files, ~7,000 lines) |
| **Delete** | `PROGRESS_TRACKER.md` (886 lines) |
| **Rewrite** | `ROADMAP.md` (1,594 → ~80 lines) |
| **Update** | `memory/MEMORY.md` (remove "Dev Progress Tracking" open question) |
| **Update** | `memory/features.md` (trim, link to issues) |

Net: ~9,500 lines removed, replaced by ~80 lines + GitHub Issues.

## Near-term Feature List

1. **Projects** — Group sessions by working directory
   - Derive project from CWD (zero config)
   - Open: TUI layout (tree view, tabbed, grouped list?)

2. **Teams/Subagents** — Parent-child session relationships
   - Hook events (SubagentStart/Stop) as primary, PID tree as fallback
   - Open: exact SubagentStart fields, nesting depth, orphan handling

3. **Filtering** — Filter by status, agent type, search
   - Keybindings for quick filters
   - `/` for search mode

4. **Preview pane** — Richer session detail view
   - Expanded info: recent tools, activity log, full context breakdown

5. **Vim navigation** — More complete keybindings
   - gg (top), G (bottom), Ctrl-d/u (half-page), etc.

6. **Session actions** — Kill session with confirmation
   - Send SIGTERM to PID, confirmation dialog

7. **Pre-select current pane** — Auto-highlight last-active session
   - TUI reads its own $TMUX_PANE on launch
   - Explore: `tmux display-message` for last-focused pane metadata
   - Hook scripts already send $TMUX_PANE per session

## Execution Steps

1. Create GitHub Issues for each near-term feature
2. Delete `atm-plan/` directory
3. Delete `PROGRESS_TRACKER.md`
4. Rewrite `ROADMAP.md` (compact version with issue links)
5. Update `memory/MEMORY.md` and `memory/features.md`
6. Commit all changes
