//! pi wire payload deserialization.
//!
//! Matches the JSONL format the pi extension API produces (verified
//! against real traces in `/tmp/atm-pi-spike-*.jsonl` from spike
//! `agent-tmux-manager-9dn`).
//!
//! The TS extension subscribes via `pi.on(eventName, handler)` and
//! forwards `{ event: <name>, payload: <object> }` to atmd. We
//! deserialize that envelope here.

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::event::PiEventType;

/// Wire envelope: pi extension forwards `{event, payload}` per event.
///
/// This is what the TS extension (`pi-atm`, bead
/// `agent-tmux-manager-6dx`) sends to atmd over the existing Unix
/// socket protocol.
#[derive(Debug, Clone, Deserialize)]
pub struct RawPiEvent {
    /// pi event name (e.g. `"agent_start"`, `"tool_execution_start"`).
    pub event: PiEventType,

    /// Event-specific payload — flat object with the fields pi
    /// includes for that event. Captured as a tagged union.
    #[serde(default)]
    pub payload: PiPayload,

    // === Fields injected by the TS extension ===
    /// Session id (resolved from pi's session manager at emission time).
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub session_id: Option<String>,
    /// Pi process pid (so atmd can correlate to /proc).
    #[serde(default)]
    pub pid: Option<u32>,
    /// Tmux pane id when running inside tmux.
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub tmux_pane: Option<String>,
}

/// Pi event payload — fields vary per event type.
///
/// Modeled as one struct with all `Option` fields because pi's runtime
/// JSON shape is essentially that: each event fills the relevant
/// fields and leaves the rest absent. This mirrors how
/// [`atm_claude_adapter::RawHookEvent`] handles Claude.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PiPayload {
    // === Tool events (tool_execution_start/end, tool_call, tool_result) ===
    #[serde(
        rename = "toolName",
        default,
        deserialize_with = "deserialize_optional_stringish"
    )]
    pub tool_name: Option<String>,
    #[serde(
        rename = "toolCallId",
        default,
        deserialize_with = "deserialize_optional_stringish"
    )]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub args: Option<serde_json::Value>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub content: Option<serde_json::Value>,
    #[serde(rename = "isError", default)]
    pub is_error: Option<bool>,

    // === Session lifecycle (session_start, session_shutdown) ===
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub reason: Option<String>,

    // === Input event (interactive prompt) ===
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub source: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub text: Option<String>,

    // === Model select ===
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub provider: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub model: Option<String>,

    // === Context event (carries cost/tokens) ===
    /// `messages[]` — full conversation snapshot.
    #[serde(default)]
    pub messages: Option<serde_json::Value>,

    // === Permission gate (synthesized by extension's tool_call handler) ===
    /// True when the upstream pi extension is signalling that pi has
    /// reached a `ctx.ui.select(...)` permission prompt and is awaiting
    /// user input. Pi itself does not emit a `block: true` payload, so
    /// this flag is set by an adapter-side TS extension and consumed
    /// here in `translate.rs` to drive `NeedsInput`.
    #[serde(default)]
    pub needs_user_input: Option<bool>,

    // === Synthetic atm events (atm_needs_input_open) ===
    /// Title/prompt of the dialog that just opened. Some extensions
    /// (pi-amplike) include the bash command being gated; the title
    /// surfaces in the TUI's activity-detail field.
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub title: Option<String>,
}

fn deserialize_optional_stringish<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(string_from_value))
}

fn string_from_value(value: Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => non_empty(s),
        Value::Number(n) => non_empty(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Array(_) => None,
        Value::Object(map) => [
            "id",
            "session_id",
            "sessionId",
            "text",
            "title",
            "label",
            "name",
            "value",
        ]
        .iter()
        .find_map(|key| map.get(*key).cloned().and_then(string_from_value)),
    }
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sample line from `/tmp/atm-pi-spike-pid1226148-2026-05-02T08-55-56-043Z.jsonl`.
    #[test]
    fn parse_session_start_real_payload() {
        let json = r#"{
            "event": "session_start",
            "payload": {"type": "session_start", "reason": "startup"}
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::SessionStart);
        assert_eq!(raw.payload.reason.as_deref(), Some("startup"));
    }

    #[test]
    fn parse_input_real_payload() {
        let json = r#"{
            "event": "input",
            "payload": {
                "type": "input",
                "text": "Use ls to list /tmp",
                "source": "interactive"
            }
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::Input);
        assert_eq!(raw.payload.source.as_deref(), Some("interactive"));
        assert_eq!(raw.payload.text.as_deref(), Some("Use ls to list /tmp"));
    }

    #[test]
    fn parse_tool_execution_start_real_payload() {
        let json = r#"{
            "event": "tool_execution_start",
            "payload": {
                "type": "tool_execution_start",
                "toolName": "ls",
                "toolCallId": "call_QPdBI",
                "args": {"path": "/tmp", "limit": 3}
            }
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::ToolExecutionStart);
        assert_eq!(raw.payload.tool_name.as_deref(), Some("ls"));
        assert_eq!(raw.payload.tool_call_id.as_deref(), Some("call_QPdBI"));
    }

    #[test]
    fn parse_tool_call_undeclared_event() {
        // tool_call is undeclared in pi's TypeScript types but real.
        let json = r#"{
            "event": "tool_call",
            "payload": {
                "type": "tool_call",
                "toolName": "ls",
                "toolCallId": "call_QPdBI",
                "input": {"path": "/tmp"}
            }
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::ToolCall);
        assert_eq!(raw.payload.tool_name.as_deref(), Some("ls"));
        assert!(raw.payload.input.is_some());
    }

    #[test]
    fn parse_tool_result_with_error_flag() {
        let json = r#"{
            "event": "tool_result",
            "payload": {
                "type": "tool_result",
                "toolName": "ls",
                "toolCallId": "call_QPdBI",
                "isError": false,
                "content": [{"type":"text","text":"a\nb\n"}]
            }
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::ToolResult);
        assert_eq!(raw.payload.is_error, Some(false));
    }

    #[test]
    fn parse_session_shutdown_with_reason() {
        let json = r#"{
            "event": "session_shutdown",
            "payload": {"type":"session_shutdown","reason":"quit"}
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::SessionShutdown);
        assert_eq!(raw.payload.reason.as_deref(), Some("quit"));
    }

    #[test]
    fn parse_unknown_event_falls_back_to_other() {
        let json = r#"{"event": "future_pi_event", "payload": {}}"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.event, PiEventType::Other("future_pi_event".to_string()));
    }

    #[test]
    fn stringish_fields_accept_object_ids() {
        // Regression shape for atmd log error:
        // `Parse error: invalid type: map, expected a string` from pi-atm.
        // Some pi extension/runtime fields are object-like even when ATM only
        // needs their stable string id. The adapter should not drop the entire
        // event on that schema drift.
        let json = r#"{
            "event": "model_select",
            "session_id": {"id": "019e3c43-c15a-740c-9a59-8faba2531283"},
            "tmux_pane": {"id": "%28"},
            "payload": {
                "type":"model_select",
                "provider": {"id":"anthropic"},
                "model": {"id":"claude-sonnet-4-6"}
            }
        }"#;

        let raw: RawPiEvent = serde_json::from_str(json).unwrap();

        assert_eq!(
            raw.session_id.as_deref(),
            Some("019e3c43-c15a-740c-9a59-8faba2531283")
        );
        assert_eq!(raw.tmux_pane.as_deref(), Some("%28"));
        assert_eq!(raw.payload.provider.as_deref(), Some("anthropic"));
        assert_eq!(raw.payload.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn stringish_payload_fields_drop_unlabeled_objects_instead_of_failing_parse() {
        let json = r#"{
            "event": "input",
            "payload": {
                "type": "input",
                "text": {"content": [{"type": "text", "text": "hello"}]},
                "source": "interactive"
            }
        }"#;

        let raw: RawPiEvent = serde_json::from_str(json).unwrap();

        assert_eq!(raw.event, PiEventType::Input);
        assert_eq!(raw.payload.text, None);
        assert_eq!(raw.payload.source.as_deref(), Some("interactive"));
    }
}
