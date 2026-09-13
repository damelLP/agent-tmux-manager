//! Raw Claude Code hook-event payload (the JSON Claude sends on stdin
//! to the hook script).
//!
//! Flat structure with all possible fields as `Option<T>` — Claude
//! emits the same envelope shape for every event and only fills in the
//! relevant fields. Use [`RawHookEvent::to_lifecycle_event`] to convert
//! into a vendor-neutral `LifecycleEvent`.

use atm_core::SessionId;
use serde::Deserialize;

use crate::event::ClaudeEventType;

/// Raw hook event JSON structure from Claude Code.
///
/// Use typed conversion ([`Self::to_lifecycle_event`]) for domain-layer
/// type safety; the daemon never matches on `hook_event_name` strings.
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
    #[serde(default)]
    pub background_tasks: Option<serde_json::Value>,
    #[serde(default)]
    pub session_crons: Option<serde_json::Value>,

    // === Subagent Events (SubagentStart, SubagentStop) ===
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub agent_type: Option<String>,
    #[serde(default)]
    pub agent_transcript_path: Option<String>,

    // === Session Events (SessionStart, SessionEnd) ===
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub model: Option<String>,

    // === Compaction/Setup (PreCompact, Setup) ===
    #[serde(default)]
    pub trigger: Option<String>,
    #[serde(default)]
    pub custom_instructions: Option<String>,

    // === Notification ===
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,

    // === Agent teams ===
    #[serde(default)]
    pub teammate_name: Option<String>,
    #[serde(default)]
    pub task_subject: Option<String>,
}

impl RawHookEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<ClaudeEventType> {
        ClaudeEventType::from_event_name(&self.hook_event_name)
    }

    /// Returns the session ID.
    pub fn session_id(&self) -> SessionId {
        SessionId::new(&self.session_id)
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
            "tool_name": "Bash"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::PreToolUse));
        assert_eq!(event.tool_name.as_deref(), Some("Bash"));
    }

    #[test]
    fn parse_stop() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "Stop",
            "stop_hook_active": true
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::Stop));
        assert_eq!(event.stop_hook_active, Some(true));
    }

    #[test]
    fn parse_user_prompt() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Help me write a function"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::UserPromptSubmit));
        assert_eq!(event.prompt.as_deref(), Some("Help me write a function"));
    }

    #[test]
    fn parse_subagent_start() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "SubagentStart",
            "agent_id": "agent_456",
            "agent_type": "Explore"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::SubagentStart));
        assert_eq!(event.agent_id.as_deref(), Some("agent_456"));
        assert_eq!(event.agent_type.as_deref(), Some("Explore"));
    }

    #[test]
    fn parse_notification() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
            "message": "Allow tool execution?"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::Notification));
        assert_eq!(
            event.notification_type.as_deref(),
            Some("permission_prompt")
        );
    }

    #[test]
    fn parse_session_start() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "SessionStart",
            "source": "resume",
            "model": "claude-opus-4-5-20251101"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::SessionStart));
        assert_eq!(event.source.as_deref(), Some("resume"));
    }

    #[test]
    fn parse_pre_compact() {
        let json = r#"{
            "session_id": "test-123",
            "hook_event_name": "PreCompact",
            "trigger": "auto"
        }"#;
        let event: RawHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(ClaudeEventType::PreCompact));
        assert_eq!(event.trigger.as_deref(), Some("auto"));
    }
}
