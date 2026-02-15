//! Hook event types and tool classification from Claude Code.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Returns true if the given tool name represents an interactive tool
/// that requires user input.
///
/// Interactive tools pause execution and wait for user response:
/// - `AskUserQuestion`: Prompts user with a question/options
/// - `EnterPlanMode`: Enters planning mode, waits for user approval
/// - `ExitPlanMode`: Presents plan for user approval
///
/// When these tools fire `PreToolUse`, the session should show the
/// "needs input" indicator (blinking yellow !) rather than "running".
///
/// # Arguments
/// * `tool_name` - The name of the tool from a PreToolUse hook event
///
/// # Returns
/// `true` if the tool is interactive and needs user input, `false` otherwise.
/// Returns `false` for empty or whitespace-only tool names.
#[must_use]
pub fn is_interactive_tool(tool_name: &str) -> bool {
    let trimmed = tool_name.trim();
    !trimmed.is_empty()
        && matches!(
            trimmed,
            "AskUserQuestion" | "EnterPlanMode" | "ExitPlanMode"
        )
}

/// All HookEventType variants paired with their string names.
/// Single source of truth for string conversion.
const HOOK_EVENT_VARIANTS: &[(HookEventType, &str)] = &[
    (HookEventType::PreToolUse, "PreToolUse"),
    (HookEventType::PostToolUse, "PostToolUse"),
    (HookEventType::PostToolUseFailure, "PostToolUseFailure"),
    (HookEventType::UserPromptSubmit, "UserPromptSubmit"),
    (HookEventType::Stop, "Stop"),
    (HookEventType::SubagentStart, "SubagentStart"),
    (HookEventType::SubagentStop, "SubagentStop"),
    (HookEventType::SessionStart, "SessionStart"),
    (HookEventType::SessionEnd, "SessionEnd"),
    (HookEventType::PreCompact, "PreCompact"),
    (HookEventType::Setup, "Setup"),
    (HookEventType::Notification, "Notification"),
];

/// Types of hook events from Claude Code.
///
/// All 12 Claude Code hook events, based on official documentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum HookEventType {
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
}

impl HookEventType {
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

impl fmt::Display for HookEventType {
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
            HookEventType::from_event_name("PreToolUse"),
            Some(HookEventType::PreToolUse)
        );
        assert_eq!(HookEventType::from_event_name("Unknown"), None);
    }

    #[test]
    fn test_hook_event_classification() {
        assert!(HookEventType::PreToolUse.is_pre_event());
        assert!(HookEventType::PostToolUse.is_post_event());
        assert!(!HookEventType::PreToolUse.is_post_event());
    }

    #[test]
    fn test_is_interactive_tool() {
        // Interactive tools should return true
        assert!(is_interactive_tool("AskUserQuestion"));
        assert!(is_interactive_tool("EnterPlanMode"));
        assert!(is_interactive_tool("ExitPlanMode"));

        // Standard tools should return false
        assert!(!is_interactive_tool("Bash"));
        assert!(!is_interactive_tool("Read"));
        assert!(!is_interactive_tool("Write"));
        assert!(!is_interactive_tool("Edit"));
        assert!(!is_interactive_tool("WebSearch"));
        assert!(!is_interactive_tool("Grep"));
        assert!(!is_interactive_tool("Glob"));
        assert!(!is_interactive_tool("Task"));
    }

    #[test]
    fn test_is_interactive_tool_edge_cases() {
        // Empty string should return false
        assert!(!is_interactive_tool(""));

        // Whitespace only should return false
        assert!(!is_interactive_tool("   "));
        assert!(!is_interactive_tool("\t"));
        assert!(!is_interactive_tool("\n"));

        // Case sensitivity - wrong case should return false
        assert!(!is_interactive_tool("askuserquestion"));
        assert!(!is_interactive_tool("ASKUSERQUESTION"));
        assert!(!is_interactive_tool("AskUserquestion"));

        // Partial matches should return false
        assert!(!is_interactive_tool("AskUser"));
        assert!(!is_interactive_tool("Question"));
        assert!(!is_interactive_tool("EnterPlan"));

        // Extra whitespace should be trimmed
        assert!(is_interactive_tool("  AskUserQuestion  "));
        assert!(is_interactive_tool("\tEnterPlanMode\n"));
    }

    #[test]
    fn test_hook_event_all_variants_parse() {
        // Tool events
        assert_eq!(
            HookEventType::from_event_name("PreToolUse"),
            Some(HookEventType::PreToolUse)
        );
        assert_eq!(
            HookEventType::from_event_name("PostToolUse"),
            Some(HookEventType::PostToolUse)
        );
        assert_eq!(
            HookEventType::from_event_name("PostToolUseFailure"),
            Some(HookEventType::PostToolUseFailure)
        );

        // User events
        assert_eq!(
            HookEventType::from_event_name("UserPromptSubmit"),
            Some(HookEventType::UserPromptSubmit)
        );
        assert_eq!(
            HookEventType::from_event_name("Stop"),
            Some(HookEventType::Stop)
        );

        // Subagent events
        assert_eq!(
            HookEventType::from_event_name("SubagentStart"),
            Some(HookEventType::SubagentStart)
        );
        assert_eq!(
            HookEventType::from_event_name("SubagentStop"),
            Some(HookEventType::SubagentStop)
        );

        // Session events
        assert_eq!(
            HookEventType::from_event_name("SessionStart"),
            Some(HookEventType::SessionStart)
        );
        assert_eq!(
            HookEventType::from_event_name("SessionEnd"),
            Some(HookEventType::SessionEnd)
        );

        // Context events
        assert_eq!(
            HookEventType::from_event_name("PreCompact"),
            Some(HookEventType::PreCompact)
        );
        assert_eq!(
            HookEventType::from_event_name("Setup"),
            Some(HookEventType::Setup)
        );

        // Notification
        assert_eq!(
            HookEventType::from_event_name("Notification"),
            Some(HookEventType::Notification)
        );
    }

    #[test]
    fn test_hook_event_classification_extended() {
        // Pre-events
        assert!(HookEventType::PreToolUse.is_pre_event());
        assert!(HookEventType::SessionStart.is_pre_event());
        assert!(HookEventType::PreCompact.is_pre_event());
        assert!(HookEventType::SubagentStart.is_pre_event());
        assert!(HookEventType::Setup.is_pre_event());

        // Post-events
        assert!(HookEventType::PostToolUse.is_post_event());
        assert!(HookEventType::PostToolUseFailure.is_post_event());
        assert!(HookEventType::SessionEnd.is_post_event());
        assert!(HookEventType::Stop.is_post_event());
        assert!(HookEventType::SubagentStop.is_post_event());
    }
}
