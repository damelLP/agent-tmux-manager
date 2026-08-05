//! Raw GitHub Copilot CLI hook-event payload (the JSON Copilot sends
//! on stdin to the hook script).
//!
//! Copilot's own per-event payloads (see
//! <https://github.com/github/copilot-sdk/blob/main/rust/src/hooks.rs>
//! for the authoritative field shapes) don't carry an explicit
//! "which event is this" field — the event kind is implied by which
//! array in `hooks.json` the command was registered under. Our
//! installed hook script (`atm-copilot-hook`, one invocation per
//! event array) adds a `hookEventName` field before forwarding to
//! atmd, mirroring how `atm-claude-adapter`'s `hook_event_name` and
//! `atm-devin-adapter`'s `hook_event_name` work — this keeps a single
//! envelope shape across all three adapters at the connection
//! boundary.

use atm_core::SessionId;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::event::CopilotEventType;

/// Raw hook event JSON structure from Copilot CLI (as forwarded by
/// `atm-copilot-hook`).
///
/// Use typed conversion ([`Self::to_lifecycle_event`]) for
/// domain-layer type safety; the daemon never matches on
/// `hook_event_name` strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawCopilotHookEvent {
    // === Common fields (injected by atm-copilot-hook) ===
    pub session_id: String,
    pub hook_event_name: String,

    // === Injected by hook script ===
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tmux_pane: Option<String>,

    // === Tool events (preToolUse, postToolUse, postToolUseFailure) ===
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Arguments passed to the tool.
    ///
    /// Documented as an object, but real CLI invocations have been
    /// observed sending a JSON-encoded *string* instead (see
    /// <https://github.com/github/copilot-cli/issues/3349>). Accept
    /// both via a custom deserializer rather than failing the whole
    /// event on a shape we don't control.
    #[serde(default, deserialize_with = "deserialize_json_ish")]
    pub tool_args: Option<Value>,
    /// Result returned by the tool (`postToolUse` only).
    #[serde(default, deserialize_with = "deserialize_json_ish")]
    pub tool_result: Option<Value>,
    /// Failure message (`postToolUseFailure` only — Copilot extracts
    /// this from the tool result rather than passing it whole).
    #[serde(default)]
    pub error: Option<String>,

    // === User prompt (userPromptSubmitted) ===
    #[serde(default)]
    pub prompt: Option<String>,

    // === Session events (sessionStart, sessionEnd) ===
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Deserializes a field that should be a JSON object but may arrive
/// as a JSON-encoded string (see [`RawCopilotHookEvent::tool_args`]).
fn deserialize_json_ish<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.map(|v| match v {
        Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        other => other,
    }))
}

impl RawCopilotHookEvent {
    /// Parses the hook event type.
    pub fn event_type(&self) -> Option<CopilotEventType> {
        CopilotEventType::from_event_name(&self.hook_event_name)
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
    fn parse_pre_tool_use_object_args() {
        let json = r#"{
            "sessionId": "s-1",
            "hookEventName": "preToolUse",
            "toolName": "bash",
            "toolArgs": {"command": "ls"}
        }"#;
        let event: RawCopilotHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type(), Some(CopilotEventType::PreToolUse));
        assert_eq!(event.tool_args, Some(serde_json::json!({"command": "ls"})));
    }

    #[test]
    fn parse_pre_tool_use_string_encoded_args() {
        // Regression for github/copilot-cli#3349: toolArgs observed as
        // a JSON-encoded string rather than a parsed object.
        let json = r#"{
            "sessionId": "s-1",
            "hookEventName": "preToolUse",
            "toolName": "bash",
            "toolArgs": "{\"command\":\"echo hello\"}"
        }"#;
        let event: RawCopilotHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            event.tool_args,
            Some(serde_json::json!({"command": "echo hello"}))
        );
    }

    #[test]
    fn parse_post_tool_use_failure() {
        let json = r#"{
            "sessionId": "s-1",
            "hookEventName": "postToolUseFailure",
            "toolName": "bash",
            "error": "command not found"
        }"#;
        let event: RawCopilotHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            event.event_type(),
            Some(CopilotEventType::PostToolUseFailure)
        );
        assert_eq!(event.error.as_deref(), Some("command not found"));
    }

    #[test]
    fn parse_user_prompt_submitted() {
        let json = r#"{
            "sessionId": "s-1",
            "hookEventName": "userPromptSubmitted",
            "prompt": "fix the tests"
        }"#;
        let event: RawCopilotHookEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.prompt.as_deref(), Some("fix the tests"));
    }

    #[test]
    fn parse_session_start_and_end() {
        let start: RawCopilotHookEvent = serde_json::from_str(
            r#"{"sessionId":"s-1","hookEventName":"sessionStart","source":"startup"}"#,
        )
        .unwrap();
        assert_eq!(start.source.as_deref(), Some("startup"));

        let end: RawCopilotHookEvent = serde_json::from_str(
            r#"{"sessionId":"s-1","hookEventName":"sessionEnd","reason":"user_exit"}"#,
        )
        .unwrap();
        assert_eq!(end.reason.as_deref(), Some("user_exit"));
    }
}
