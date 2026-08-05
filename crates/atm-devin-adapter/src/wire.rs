//! Raw Devin CLI hook-event payload (the JSON Devin sends on stdin to
//! the hook script).
//!
//! Flat structure with all possible fields as `Option<T>` — mirrors
//! [`atm_claude_adapter::wire::RawHookEvent`] since Devin's hook
//! envelope shape is close to Claude's. Use
//! [`RawDevinHookEvent::to_lifecycle_event`] to convert into a
//! vendor-neutral `LifecycleEvent`.

use atm_core::SessionId;
use serde::Deserialize;

use crate::event::DevinEventType;

/// Raw hook event JSON structure from Devin CLI.
///
/// Use typed conversion ([`Self::to_lifecycle_event`]) for domain-layer
/// type safety; the daemon never matches on `hook_event_name` strings.
#[derive(Debug, Clone, Deserialize)]
pub struct RawDevinHookEvent {
    // === Common Fields (all events) ===
    pub session_id: String,
    pub hook_event_name: String,
    /// Per-turn id, rotated on every user prompt. Absent for events
    /// that fire before the first user prompt (e.g. `SessionStart`).
    #[serde(default)]
    pub prompt_id: Option<String>,

    // === Injected by hook script ===
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tmux_pane: Option<String>,

    // === Tool Events (PreToolUse, PostToolUse, PermissionRequest) ===
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<serde_json::Value>,
    #[serde(default)]
    pub tool_response: Option<serde_json::Value>,

    // === User Prompt (UserPromptSubmit) ===
    #[serde(default)]
    pub prompt: Option<String>,

    // === Stop ===
    #[serde(default)]
    pub stop_hook_active: Option<bool>,

    // === PostCompaction ===
    #[serde(default)]
    pub summary: Option<String>,

    // === Session Events (SessionStart, SessionEnd) ===
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

impl RawDevinHookEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<DevinEventType> {
        DevinEventType::from_event_name(&self.hook_event_name)
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> SessionId {
        SessionId::new(&self.session_id)
    }

    /// Returns true if `tool_response` reports failure.
    ///
    /// Devin, unlike Claude, does not split tool failure into a
    /// separate `PostToolUseFailure` event — `PostToolUse` fires for
    /// both outcomes, and `tool_response.success` distinguishes them.
    /// Defaults to `false` (success) when absent or malformed, since
    /// a missing field shouldn't be treated as a failure signal.
    #[must_use]
    pub fn tool_failed(&self) -> bool {
        self.tool_response
            .as_ref()
            .and_then(|r| r.get("success"))
            .and_then(serde_json::Value::as_bool)
            .map(|success| !success)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pre_tool_use() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PreToolUse",
            "tool_name": "exec",
            "tool_input": {"command": "ls"}
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(DevinEventType::PreToolUse));
        assert_eq!(event.tool_name.as_deref(), Some("exec"));
    }

    #[test]
    fn parse_post_tool_use_success() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_response": {"success": true, "output": "ok", "error": null}
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert!(!event.tool_failed());
    }

    #[test]
    fn parse_post_tool_use_failure() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PostToolUse",
            "tool_name": "exec",
            "tool_response": {"success": false, "output": "", "error": "boom"}
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert!(event.tool_failed());
    }

    #[test]
    fn parse_permission_request() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PermissionRequest",
            "tool_name": "exec",
            "tool_input": {"command": "rm -rf /"}
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(DevinEventType::PermissionRequest));
    }

    #[test]
    fn parse_user_prompt_submit() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "fix the tests"
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.prompt.as_deref(), Some("fix the tests"));
    }

    #[test]
    fn parse_post_compaction() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PostCompaction",
            "summary": "Compacted 40 messages"
        }"#;
        let event: RawDevinHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.summary.as_deref(), Some("Compacted 40 messages"));
    }

    #[test]
    fn parse_session_start_and_end() {
        let start: RawDevinHookEvent = serde_json::from_str(
            r#"{"session_id":"s","hook_event_name":"SessionStart","source":"resume"}"#,
        )
        .unwrap();
        assert_eq!(start.source.as_deref(), Some("resume"));

        let end: RawDevinHookEvent = serde_json::from_str(
            r#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"user_exit"}"#,
        )
        .unwrap();
        assert_eq!(end.reason.as_deref(), Some("user_exit"));
    }
}
