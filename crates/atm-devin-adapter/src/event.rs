//! Hook event types from Devin CLI.
//!
//! `DevinEventType` is the Devin-specific raw event vocabulary. The
//! daemon never matches on it directly — `RawDevinHookEvent::to_lifecycle_event()`
//! translates it into a vendor-neutral `LifecycleEvent` at the
//! connection boundary.
//!
//! Devin CLI's hook system (see
//! <https://docs.devin.ai/cli/extensibility/hooks/overview>) is
//! close to Claude Code's — same JSON-on-stdin shape, same
//! `session_id` correlation id — but the event vocabulary differs
//! slightly: Devin has `PermissionRequest` as its own event (Claude
//! folds permission gating into `PreToolUse`/`Notification`) and
//! `PostCompaction` instead of `PreCompact`, and has no
//! `SubagentStart`/`SubagentStop`/`Setup`/`Notification` events.

use serde::{Deserialize, Serialize};
use std::fmt;

/// All `DevinEventType` variants paired with their string names.
/// Single source of truth for string conversion.
const HOOK_EVENT_VARIANTS: &[(DevinEventType, &str)] = &[
    (DevinEventType::PreToolUse, "PreToolUse"),
    (DevinEventType::PostToolUse, "PostToolUse"),
    (DevinEventType::PermissionRequest, "PermissionRequest"),
    (DevinEventType::UserPromptSubmit, "UserPromptSubmit"),
    (DevinEventType::Stop, "Stop"),
    (DevinEventType::PostCompaction, "PostCompaction"),
    (DevinEventType::SessionStart, "SessionStart"),
    (DevinEventType::SessionEnd, "SessionEnd"),
];

/// Types of hook events from Devin CLI.
///
/// All 8 documented Devin CLI hook events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum DevinEventType {
    // === Tool Execution ===
    /// Before a tool is executed.
    PreToolUse,
    /// After a tool finishes (success or failure — Devin does not
    /// split this into a separate failure event like Claude does).
    PostToolUse,

    // === Permissions ===
    /// A permission decision is needed for a tool call.
    PermissionRequest,

    // === User Interaction ===
    /// User submitted a prompt.
    UserPromptSubmit,
    /// Devin stopped responding (finished its turn).
    Stop,

    // === Context Management ===
    /// Context compaction just completed.
    PostCompaction,

    // === Session Lifecycle ===
    /// Session started (new, resumed, or startup).
    SessionStart,
    /// Session ended.
    SessionEnd,
}

impl DevinEventType {
    /// Returns the canonical string name for this event type.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        for (variant, name) in HOOK_EVENT_VARIANTS {
            if variant == self {
                return name;
            }
        }
        "Unknown"
    }

    /// Parses from a hook event name string.
    #[must_use]
    pub fn from_event_name(name: &str) -> Option<Self> {
        HOOK_EVENT_VARIANTS
            .iter()
            .find(|(_, s)| *s == name)
            .map(|(v, _)| *v)
    }
}

impl fmt::Display for DevinEventType {
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
            assert_eq!(DevinEventType::from_event_name(name), Some(*variant));
        }
        assert_eq!(DevinEventType::from_event_name("NotARealEvent"), None);
    }

    #[test]
    fn test_display_matches_as_str() {
        assert_eq!(DevinEventType::PreToolUse.to_string(), "PreToolUse");
        assert_eq!(
            DevinEventType::PermissionRequest.to_string(),
            "PermissionRequest"
        );
    }
}
