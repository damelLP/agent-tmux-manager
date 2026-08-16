//! Hook event types from the OpenAI Codex CLI.
//!
//! `CodexEventType` is the Codex-specific raw event vocabulary. The
//! daemon never matches on it directly — `RawCodexEvent::to_lifecycle_event()`
//! translates it into a vendor-neutral `LifecycleEvent` at the
//! connection boundary.

use serde::{Deserialize, Serialize};
use std::fmt;

/// All CodexEventType variants paired with their string names.
/// Single source of truth for string conversion.
const HOOK_EVENT_VARIANTS: &[(CodexEventType, &str)] = &[
    (CodexEventType::PreToolUse, "PreToolUse"),
    (CodexEventType::PostToolUse, "PostToolUse"),
    (CodexEventType::PermissionRequest, "PermissionRequest"),
    (CodexEventType::UserPromptSubmit, "UserPromptSubmit"),
    (CodexEventType::Stop, "Stop"),
    (CodexEventType::SubagentStart, "SubagentStart"),
    (CodexEventType::SubagentStop, "SubagentStop"),
    (CodexEventType::SessionStart, "SessionStart"),
    (CodexEventType::SessionEnd, "SessionEnd"),
    (CodexEventType::PreCompact, "PreCompact"),
    (CodexEventType::PostCompact, "PostCompact"),
];

/// Types of hook events from the Codex CLI.
///
/// All 11 Codex hook events, per the official hooks documentation and
/// validated against live codex-cli 0.146.1 traffic. Unlike Claude,
/// Codex has no `PostToolUseFailure`, `Setup`, or `Notification`
/// events; it adds `PermissionRequest` and `PostCompact` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CodexEventType {
    // === Tool Execution ===
    /// Before a tool is executed
    PreToolUse,
    /// After a tool completes (success or failure — Codex does not
    /// distinguish in the hook payload)
    PostToolUse,
    /// Codex is asking the user to approve a tool invocation
    PermissionRequest,

    // === User Interaction ===
    /// User submitted a prompt
    UserPromptSubmit,
    /// Codex stopped responding (finished turn)
    Stop,

    // === Subagent Lifecycle ===
    /// A subagent was spawned
    SubagentStart,
    /// A subagent completed
    SubagentStop,

    // === Session Lifecycle ===
    /// Session started (startup, resumed, cleared, or compacted)
    SessionStart,
    /// Session ended
    SessionEnd,

    // === Context Management ===
    /// Context compaction is about to occur
    PreCompact,
    /// Context compaction finished
    PostCompact,
}

impl CodexEventType {
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
            Self::PreToolUse | Self::SessionStart | Self::PreCompact | Self::SubagentStart
        )
    }

    /// Returns true if this is a post-execution event.
    #[must_use]
    pub fn is_post_event(&self) -> bool {
        matches!(
            self,
            Self::PostToolUse
                | Self::PostCompact
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

impl fmt::Display for CodexEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_variants_round_trip_through_names() {
        for (variant, name) in HOOK_EVENT_VARIANTS {
            assert_eq!(CodexEventType::from_event_name(name), Some(*variant));
            assert_eq!(variant.as_str(), *name);
        }
        assert_eq!(CodexEventType::from_event_name("NotARealEvent"), None);
    }

    #[test]
    fn classification_covers_every_variant_at_most_once() {
        for (variant, name) in HOOK_EVENT_VARIANTS {
            assert!(
                !(variant.is_pre_event() && variant.is_post_event()),
                "{name} classified as both pre and post"
            );
        }
        // Spot-checks
        assert!(CodexEventType::PreToolUse.is_pre_event());
        assert!(CodexEventType::PostToolUse.is_post_event());
        assert!(CodexEventType::PostCompact.is_post_event());
        // PermissionRequest and UserPromptSubmit are neither pre nor post.
        assert!(!CodexEventType::PermissionRequest.is_pre_event());
        assert!(!CodexEventType::PermissionRequest.is_post_event());
        assert!(!CodexEventType::UserPromptSubmit.is_pre_event());
        assert!(!CodexEventType::UserPromptSubmit.is_post_event());
    }

    #[test]
    fn serde_uses_pascal_case_wire_names() {
        assert_eq!(
            serde_json::to_string(&CodexEventType::PermissionRequest).unwrap(),
            "\"PermissionRequest\""
        );
        let parsed: CodexEventType = serde_json::from_str("\"PostCompact\"").unwrap();
        assert_eq!(parsed, CodexEventType::PostCompact);
    }
}
