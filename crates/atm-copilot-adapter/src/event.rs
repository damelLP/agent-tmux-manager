//! Hook event types from GitHub Copilot CLI.
//!
//! `CopilotEventType` is the Copilot-specific raw event vocabulary.
//! The daemon never matches on it directly —
//! `RawCopilotHookEvent::to_lifecycle_event()` translates it into a
//! vendor-neutral `LifecycleEvent` at the connection boundary.
//!
//! Unlike Claude Code and Devin CLI (`PascalCase`, snake_case fields),
//! Copilot CLI's hook configuration keys and event data use
//! `camelCase` (see
//! <https://docs.github.com/en/copilot/reference/hooks-reference>).

use serde::{Deserialize, Serialize};
use std::fmt;

/// All `CopilotEventType` variants paired with their wire string names
/// (the hook config's event key, e.g. `"preToolUse"`).
const HOOK_EVENT_VARIANTS: &[(CopilotEventType, &str)] = &[
    (CopilotEventType::PreToolUse, "preToolUse"),
    (CopilotEventType::PostToolUse, "postToolUse"),
    (CopilotEventType::PostToolUseFailure, "postToolUseFailure"),
    (CopilotEventType::PermissionRequest, "permissionRequest"),
    (CopilotEventType::UserPromptSubmitted, "userPromptSubmitted"),
    (CopilotEventType::SessionStart, "sessionStart"),
    (CopilotEventType::SessionEnd, "sessionEnd"),
];

/// Types of hook events from GitHub Copilot CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CopilotEventType {
    // === Tool Execution ===
    /// Before a tool executes.
    PreToolUse,
    /// After a tool executes successfully.
    PostToolUse,
    /// After a tool execution whose result was a failure.
    PostToolUseFailure,

    // === Permissions ===
    /// A permission decision is needed for a tool call.
    PermissionRequest,

    // === User Interaction ===
    /// User submitted a message.
    UserPromptSubmitted,

    // === Session Lifecycle ===
    /// Session begins.
    SessionStart,
    /// Session ends.
    SessionEnd,
}

impl CopilotEventType {
    /// Returns the canonical wire-string name (hook config event key)
    /// for this event type.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        for (variant, name) in HOOK_EVENT_VARIANTS {
            if variant == self {
                return name;
            }
        }
        "unknown"
    }

    /// Parses from a hook event name string (the hook config's event
    /// key, e.g. `"preToolUse"`).
    #[must_use]
    pub fn from_event_name(name: &str) -> Option<Self> {
        HOOK_EVENT_VARIANTS
            .iter()
            .find(|(_, s)| *s == name)
            .map(|(v, _)| *v)
    }
}

impl fmt::Display for CopilotEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hook_event_all_variants_parse() {
        for (variant, name) in HOOK_EVENT_VARIANTS {
            assert_eq!(CopilotEventType::from_event_name(name), Some(*variant));
        }
        assert_eq!(CopilotEventType::from_event_name("notARealEvent"), None);
    }

    #[test]
    fn test_display_matches_as_str() {
        assert_eq!(CopilotEventType::PreToolUse.to_string(), "preToolUse");
        assert_eq!(
            CopilotEventType::PostToolUseFailure.to_string(),
            "postToolUseFailure"
        );
    }
}
