# Agent Tmux Manager Integration Test

This directory contains **CORRECTED** test scripts to validate Claude Code's Status Line API and Hooks system.

⚠️ **The original implementation plan made incorrect assumptions.** These scripts are based on official Claude Code documentation.

## Purpose

Before implementing the full ATM daemon, we validated:
1. ✅ Status Line Component - Works, but provides **different data** than assumed
2. ✅ Hooks System - Works, but uses **different event names** than assumed
3. ✅ Session identification - Available via `session_id` field in JSON

## Critical Corrections

- ❌ **No `context_window` data** - Status Line does NOT provide token usage
- ❌ **No `PermissionRequest` event** - Use `PreToolUse` instead
- ✅ **Session ID available** - In JSON `session_id` field (not environment variable)
- ✅ Scripts use `input=$(cat)` pattern (not `while read` loop)

See `CLAUDE_CODE_INTEGRATION.md` for full findings.

## Test Setup

The following files have been created:
- `test-status-line.sh` - Tests Status Line API (CORRECTED)
- `test-hooks.sh` - Tests Hooks system (CORRECTED)
- `.claude/settings.json` - Configuration for Status Line
- `.claude/hooks.json` - Configuration for Hooks (with valid events)

## How to Test

### 1. Start Claude Code in this directory

```bash
cd integration-test
claude
```

### 2. Have a conversation to trigger events

Try these actions in the Claude Code session:
- Ask Claude to read a file (triggers status updates)
- Ask Claude to run a bash command (triggers PreToolUse/PostToolUse hooks)
- Ask Claude to edit a file (triggers PermissionRequest hook)

### 3. Check the log files

```bash
# View status line logs
tail -f /tmp/atm-status-test.log

# View hooks logs
tail -f /tmp/atm-hooks-test.log
```

## Expected Results

### Status Line Test
- Log file should contain JSON updates approximately every 300ms
- JSON structure should include:
  - `context_window.used_percentage`
  - `context_window.total_input_tokens`
  - `context_window.total_output_tokens`
  - `cost.total_cost_usd`
  - `model.id`
- Session ID information (environment variables, PIDs)

### Hooks Test
- Log file should contain JSON for each hook event
- JSON structure should include:
  - `hook_event_name` (PermissionRequest, PreToolUse, PostToolUse)
  - `session_id` (if available)
  - `tool_name` (for PermissionRequest)

## What to Document

After testing, document in `CLAUDE_CODE_INTEGRATION.md`:
1. Actual JSON structures received
2. Update frequency (is it really ~300ms?)
3. Available session identification methods
4. Any deviations from assumptions in the plan
5. Any missing or unexpected fields

## Cleanup

To clean up test logs:
```bash
rm /tmp/atm-status-test.log /tmp/atm-hooks-test.log /tmp/atm-status-test-init
```
