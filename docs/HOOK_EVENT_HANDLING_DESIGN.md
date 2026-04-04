# Hook Event Handling Design

This document specifies how Claude Code hook events map to session states in agent-tmux-manager.

## SessionStatus: The Three Fundamental States

Every session is in exactly one of these states at any time:

```rust
pub enum SessionStatus {
    /// Nothing happening - Claude is waiting for user's next action.
    /// User can take their time, no urgency.
    Idle,

    /// Claude is actively processing - user just waits.
    /// Work is happening, no user action needed.
    Working {
        /// Optional detail for display (e.g., "Thinking", "Running Bash")
        detail: Option<String>,
    },

    /// User must take action for the session to proceed.
    /// Something is blocked waiting for user input.
    AttentionNeeded {
        /// What's needed (e.g., "Permission for Bash")
        reason: String,
    },
}
```

### State Semantics

| State | Meaning | User Action? | UI Indicator |
|-------|---------|--------------|--------------|
| `Idle` | Claude finished, waiting for user | No urgency | Relaxed/dim |
| `Working` | Claude is busy | Just wait | Active/animated |
| `AttentionNeeded` | Blocked on user | Yes, act now | Blinking/urgent |

### Staleness (Computed Property)

Staleness is NOT a state - it's a lifecycle property based on `last_activity` timestamp:

```rust
impl SessionDomain {
    pub fn is_stale(&self) -> bool {
        self.time_since_activity() > Duration::hours(8)
    }
}
```

---

## Hook Event → SessionStatus Mapping

### Tool Execution Events

| Hook Event | Condition | → SessionStatus | Rationale |
|------------|-----------|-----------------|-----------|
| `PreToolUse` | Interactive tool* | `AttentionNeeded { reason: "{tool_name}" }` | User must respond to question/approve plan |
| `PreToolUse` | Standard tool | `Working { detail: Some("{tool_name}") }` | Tool is executing |
| `PostToolUse` | — | `Working { detail: Some("Thinking") }` | Tool done, Claude processing result |
| `PostToolUseFailure` | — | `Working { detail: Some("Thinking") }` | Tool failed, Claude deciding next step |

*Interactive tools: `AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`

### User Interaction Events

| Hook Event | Condition | → SessionStatus | Rationale |
|------------|-----------|-----------------|-----------|
| `UserPromptSubmit` | — | `Working { detail: None }` | User submitted, Claude processing |
| `Stop` | — | `Idle` | Claude finished responding, user's turn |

### Subagent Events

| Hook Event | Condition | → SessionStatus | Additional Action |
|------------|-----------|-----------------|-------------------|
| `SubagentStart` | — | `Working { detail: Some("{agent_type}") }` | Create child session |
| `SubagentStop` | — | `Working { detail: Some("Thinking") }` | Mark child as complete |

### Session Lifecycle Events

| Hook Event | Condition | → SessionStatus | Rationale |
|------------|-----------|-----------------|-----------|
| `SessionStart` | — | `Idle` | Session started, waiting for user |
| `SessionEnd` | — | *(Remove session)* | Session terminated |

### Context Management Events

| Hook Event | Condition | → SessionStatus | Rationale |
|------------|-----------|-----------------|-----------|
| `PreCompact` | — | `Working { detail: Some("Compacting") }` | Context being reduced |
| `Setup` | — | `Working { detail: Some("Setup") }` | One-time setup running |

### Notification Events

| Hook Event | `notification_type` | → SessionStatus | Rationale |
|------------|---------------------|-----------------|-----------|
| `Notification` | `permission_prompt` | `AttentionNeeded { reason: "Permission" }` | Permission dialog shown |
| `Notification` | `idle_prompt` | `Idle` | Claude waiting 60+ seconds |
| `Notification` | `elicitation_dialog` | `AttentionNeeded { reason: "MCP input" }` | MCP tool needs user input |
| `Notification` | `auth_success` | *(No change)* | Informational only |
| `Notification` | unknown/missing | *(No change)* | Safe fallback |

---

## Complete Field Capture

### RawHookEvent Structure

All fields that can appear in any hook event:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct RawHookEvent {
    // === Common Fields (all events) ===
    pub session_id: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,

    // === Injected by hook script ===
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tmux_pane: Option<String>,

    // === Tool Events (PreToolUse, PostToolUse, PostToolUseFailure) ===
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_use_id: Option<String>,

    // === User Prompt (UserPromptSubmit) ===
    #[serde(default)]
    pub prompt: Option<String>,

    // === Stop Events (Stop, SubagentStop) ===
    #[serde(default)]
    pub stop_hook_active: Option<bool>,

    // === Subagent Events (SubagentStart, SubagentStop) ===
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    // === Session Events (SessionStart, SessionEnd) ===
    #[serde(default)]
    pub source: Option<String>,      // startup/resume/clear/compact
    #[serde(default)]
    pub reason: Option<String>,      // clear/logout/prompt_input_exit/other
    #[serde(default)]
    pub model: Option<String>,

    // === Compaction (PreCompact) ===
    #[serde(default)]
    pub trigger: Option<String>,     // manual/auto (also Setup: init/maintenance)
    #[serde(default)]
    pub custom_instructions: Option<String>,

    // === Notification ===
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}
```

### Fields by Event Type

| Event | Required Fields | Optional Fields |
|-------|-----------------|-----------------|
| `PreToolUse` | session_id, hook_event_name, tool_name | tool_input, tool_use_id, cwd, permission_mode |
| `PostToolUse` | session_id, hook_event_name, tool_name | tool_input, tool_response, tool_use_id |
| `PostToolUseFailure` | session_id, hook_event_name, tool_name | tool_input, tool_response, tool_use_id |
| `UserPromptSubmit` | session_id, hook_event_name, prompt | cwd, permission_mode |
| `Stop` | session_id, hook_event_name | stop_hook_active |
| `SubagentStart` | session_id, hook_event_name, agent_id, agent_type | cwd, permission_mode |
| `SubagentStop` | session_id, hook_event_name, agent_id | agent_transcript_path, stop_hook_active |
| `SessionStart` | session_id, hook_event_name | source, model, agent_type |
| `SessionEnd` | session_id, hook_event_name | reason |
| `PreCompact` | session_id, hook_event_name | trigger, custom_instructions |
| `Setup` | session_id, hook_event_name | trigger |
| `Notification` | session_id, hook_event_name | notification_type, message |

---

## Subagent Tracking

### Design: Subagents as Child Sessions

Subagents are tracked as child sessions linked to their parent:

```rust
pub struct SessionDomain {
    pub id: SessionId,

    // Parent-child relationship for subagents
    pub parent_session_id: Option<SessionId>,  // None for main sessions
    pub agent_id: Option<String>,              // For subagents: unique ID

    // ... rest of existing fields ...
}
```

### Hierarchy Example

```
Main Session (session_id: "abc123", status: Working)
├── Subagent (agent_id: "agent_456", agent_type: "Explore", status: Working)
├── Subagent (agent_id: "agent_789", agent_type: "Plan", status: Idle)
└── Subagent (agent_id: "agent_012", agent_type: "CodeReviewer", status: Working)
```

### Subagent Lifecycle

| Event | Action |
|-------|--------|
| `SubagentStart` | Create child session with `parent_session_id` = current session |
| `SubagentStop` | Set child status to `Idle` (or optionally remove) |

### Registry Changes

```rust
// Primary storage unchanged
sessions_by_pid: HashMap<u32, (SessionDomain, SessionInfrastructure)>,

// New index for subagent lookup
sessions_by_agent_id: HashMap<String, SessionId>,  // agent_id → session_id
```

### Future UI Possibilities

```
┌─────────────────────────────────────────────────────┐
│ abc123 (Opus 4.5)                    Working        │
│ ├─ Explore                           Working        │
│ ├─ Plan                              Idle           │
│ └─ CodeReviewer                      Working        │
│ Context: 45% │ Cost: $0.35 │ Duration: 2m 15s      │
└─────────────────────────────────────────────────────┘
```

---

## HookEventType Enum

All 12 Claude Code hook events:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEventType {
    // Tool Execution
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,

    // User Interaction
    UserPromptSubmit,
    Stop,

    // Subagent Lifecycle
    SubagentStart,
    SubagentStop,

    // Session Lifecycle
    SessionStart,
    SessionEnd,

    // Context Management
    PreCompact,
    Setup,

    // Notifications
    Notification,
}
```

---

## Implementation Files

| File | Changes |
|------|---------|
| `atm-core/src/hook.rs` | Add 7 new event types to `HookEventType` |
| `atm-core/src/session.rs` | Replace 6-variant `SessionStatus` with 3-variant; add parent/agent fields |
| `atm-protocol/src/parse.rs` | Expand `RawHookEvent` with all fields |
| `atmd/registry/commands.rs` | Update `ApplyHookEvent` command with new fields |
| `atmd/registry/actor.rs` | Handle subagent creation/tracking; implement new state mapping |
| `atmd/registry/handle.rs` | Update public API |
| `atmd/server/connection.rs` | Extract and pass new fields |

---

## References

- [Claude Code Hooks Reference](./CLAUDE_CODE_HOOKS_REFERENCE.md)
- [Claude Code Integration Notes](../integration-test/CLAUDE_CODE_INTEGRATION.md)
