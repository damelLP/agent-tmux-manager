//! Hook event types from Claude Code.
//!
//! `ClaudeEventType` is the Claude-specific raw event vocabulary. The
//! daemon never matches on it directly — `RawHookEvent::to_lifecycle_event()`
//! translates it into a vendor-neutral `LifecycleEvent` at the
//! connection boundary. Interactive-tool classification now lives on
//! `Tool::is_interactive` (see `crate::tool`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// All ClaudeEventType variants paired with their string names.
/// Single source of truth for string conversion.
const HOOK_EVENT_VARIANTS: &[(ClaudeEventType, &str)] = &[
    (ClaudeEventType::PreToolUse, "PreToolUse"),
    (ClaudeEventType::PostToolUse, "PostToolUse"),
    (ClaudeEventType::PostToolUseFailure, "PostToolUseFailure"),
    (ClaudeEventType::UserPromptSubmit, "UserPromptSubmit"),
    (ClaudeEventType::Stop, "Stop"),
    (ClaudeEventType::SubagentStart, "SubagentStart"),
    (ClaudeEventType::SubagentStop, "SubagentStop"),
    (ClaudeEventType::SessionStart, "SessionStart"),
    (ClaudeEventType::SessionEnd, "SessionEnd"),
    (ClaudeEventType::PreCompact, "PreCompact"),
    (ClaudeEventType::Setup, "Setup"),
    (ClaudeEventType::Notification, "Notification"),
    (ClaudeEventType::PermissionRequest, "PermissionRequest"),
    (ClaudeEventType::TeammateIdle, "TeammateIdle"),
    (ClaudeEventType::TaskCreated, "TaskCreated"),
    (ClaudeEventType::TaskCompleted, "TaskCompleted"),
];

/// Types of hook events from Claude Code.
///
/// The Claude Code hook events ATM subscribes to, based on the official
/// hooks reference. Claude emits more (e.g. `PostToolBatch`,
/// `ConfigChange`); unknown names translate to `None` and are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ClaudeEventType {
    // === Tool Execution ===
    /// Before a tool is executed
    PreToolUse,
    /// After a tool completes successfully
    PostToolUse,
    /// After a tool fails
    PostToolUseFailure,

    // === User Interaction ===
    /// User submitted a prompt
    UserPromptSubmit,
    /// Claude stopped responding (finished turn)
    Stop,

    // === Subagent Lifecycle ===
    /// A subagent was spawned
    SubagentStart,
    /// A subagent completed
    SubagentStop,

    // === Session Lifecycle ===
    /// Session started (new, resumed, or cleared)
    SessionStart,
    /// Session ended
    SessionEnd,

    // === Context Management ===
    /// Context compaction is about to occur
    PreCompact,
    /// One-time setup is running
    Setup,

    // === Notifications ===
    /// Informational notification
    Notification,

    // === Permission ===
    /// A tool call needs a permission decision (fires immediately,
    /// unlike `Notification(permission_prompt)` which waits ~6s).
    PermissionRequest,

    // === Agent Teams ===
    /// A teammate is about to go idle
    TeammateIdle,
    /// A shared task-list entry was created via `TaskCreate`
    TaskCreated,
    /// A shared task-list entry was marked completed
    TaskCompleted,
}

impl ClaudeEventType {
    /// Returns the canonical string name for this event type.
    ///
    /// This is the single source of truth for event name strings,
    /// used by both `from_event_name()` and `Display`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        // Use the constant array as single source of truth
        for (variant, name) in HOOK_EVENT_VARIANTS {
            if variant == self {
                return name;
            }
        }
        // This is unreachable if HOOK_EVENT_VARIANTS is complete
        "Unknown"
    }

    /// Returns true if this is a pre-execution event.
    #[must_use]
    pub fn is_pre_event(&self) -> bool {
        matches!(
            self,
            Self::PreToolUse
                | Self::SessionStart
                | Self::PreCompact
                | Self::SubagentStart
                | Self::Setup
        )
    }

    /// Returns true if this is a post-execution event.
    #[must_use]
    pub fn is_post_event(&self) -> bool {
        matches!(
            self,
            Self::PostToolUse
                | Self::PostToolUseFailure
                | Self::SessionEnd
                | Self::Stop
                | Self::SubagentStop
        )
    }

    /// Parses from a hook event name string.
    ///
    /// Uses the `HOOK_EVENT_VARIANTS` constant as single source of truth.
    #[must_use]
    pub fn from_event_name(name: &str) -> Option<Self> {
        HOOK_EVENT_VARIANTS
            .iter()
            .find(|(_, s)| *s == name)
            .map(|(v, _)| *v)
    }
}

impl fmt::Display for ClaudeEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_parsing() {
        assert_eq!(
            ClaudeEventType::from_event_name("PreToolUse"),
            Some(ClaudeEventType::PreToolUse)
        );
        assert_eq!(ClaudeEventType::from_event_name("Unknown"), None);
    }

    #[test]
    fn test_hook_event_classification() {
        assert!(ClaudeEventType::PreToolUse.is_pre_event());
        assert!(ClaudeEventType::PostToolUse.is_post_event());
        assert!(!ClaudeEventType::PreToolUse.is_post_event());
    }

    #[test]
    fn test_hook_event_all_variants_parse() {
        // Tool events
        assert_eq!(
            ClaudeEventType::from_event_name("PreToolUse"),
            Some(ClaudeEventType::PreToolUse)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("PostToolUse"),
            Some(ClaudeEventType::PostToolUse)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("PostToolUseFailure"),
            Some(ClaudeEventType::PostToolUseFailure)
        );

        // User events
        assert_eq!(
            ClaudeEventType::from_event_name("UserPromptSubmit"),
            Some(ClaudeEventType::UserPromptSubmit)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("Stop"),
            Some(ClaudeEventType::Stop)
        );

        // Subagent events
        assert_eq!(
            ClaudeEventType::from_event_name("SubagentStart"),
            Some(ClaudeEventType::SubagentStart)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("SubagentStop"),
            Some(ClaudeEventType::SubagentStop)
        );

        // Session events
        assert_eq!(
            ClaudeEventType::from_event_name("SessionStart"),
            Some(ClaudeEventType::SessionStart)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("SessionEnd"),
            Some(ClaudeEventType::SessionEnd)
        );

        // Context events
        assert_eq!(
            ClaudeEventType::from_event_name("PreCompact"),
            Some(ClaudeEventType::PreCompact)
        );
        assert_eq!(
            ClaudeEventType::from_event_name("Setup"),
            Some(ClaudeEventType::Setup)
        );

        // Notification
        assert_eq!(
            ClaudeEventType::from_event_name("Notification"),
            Some(ClaudeEventType::Notification)
        );

        // Permission + agent-team events
        for (name, expected) in [
            ("PermissionRequest", ClaudeEventType::PermissionRequest),
            ("TeammateIdle", ClaudeEventType::TeammateIdle),
            ("TaskCreated", ClaudeEventType::TaskCreated),
            ("TaskCompleted", ClaudeEventType::TaskCompleted),
        ] {
            assert_eq!(ClaudeEventType::from_event_name(name), Some(expected));
            assert_eq!(expected.as_str(), name);
        }
    }

    #[test]
    fn test_hook_event_classification_extended() {
        // Pre-events
        assert!(ClaudeEventType::PreToolUse.is_pre_event());
        assert!(ClaudeEventType::SessionStart.is_pre_event());
        assert!(ClaudeEventType::PreCompact.is_pre_event());
        assert!(ClaudeEventType::SubagentStart.is_pre_event());
        assert!(ClaudeEventType::Setup.is_pre_event());

        // Post-events
        assert!(ClaudeEventType::PostToolUse.is_post_event());
        assert!(ClaudeEventType::PostToolUseFailure.is_post_event());
        assert!(ClaudeEventType::SessionEnd.is_post_event());
        assert!(ClaudeEventType::Stop.is_post_event());
        assert!(ClaudeEventType::SubagentStop.is_post_event());
    }
}
