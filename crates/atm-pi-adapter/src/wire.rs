//! pi wire payload deserialization.
//!
//! Matches the JSONL format the pi extension API produces.
//! Captured: pi, version not recorded, 2026-05-02 spike trace (see the
//! crate docs). Re-captured with pi 0.73.1 on 2026-10-24:
//! `tests/fixtures/pi_tool_run_0.73.1.jsonl`.
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
    /// pi's session id, which pi-atm reads with
    /// `ctx.sessionManager.getSessionId()`. Older pi-atm never sent it
    /// (see the crate docs).
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub session_id: Option<String>,
    /// Pi process pid (so atmd can correlate to /proc).
    #[serde(default)]
    pub pid: Option<u32>,
    /// Tmux pane id when running inside tmux.
    #[serde(default, deserialize_with = "deserialize_optional_stringish")]
    pub tmux_pane: Option<String>,
    /// pi's current model when the event fired, as pi-atm snapshots it
    /// (`{id, provider, contextWindow}` from `ctx.model`). Older pi-atm
    /// never sent it. Not `payload.model`, which is model_select's new
    /// model.
    #[serde(
        rename = "model",
        default,
        deserialize_with = "deserialize_optional_model"
    )]
    pub current_model: Option<PiModel>,
    /// pi's session totals when the event fired, as pi-atm sums them for
    /// `context` and `turn_end` frames. Older pi-atm never sent them.
    #[serde(default, deserialize_with = "deserialize_optional_usage")]
    pub usage: Option<PiUsage>,
}

/// pi-atm's usage snapshot, `{cost, tokens, contextTokens}`. pi-atm sums
/// every assistant message in the session file, as pi's footer does.
/// Captured: pi 0.73.1, `tests/fixtures/pi_session_cost_0.73.1.jsonl`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PiUsage {
    /// The session's cost in USD: the sum of `usage.cost.total`.
    pub cost_usd: Option<f64>,
    /// The session's tokens: the sum of input, output, cacheRead and
    /// cacheWrite, as pi's `/session` counts them.
    pub tokens: Option<u64>,
    /// The active context, from pi's `getContextUsage()`. `None` when pi
    /// doesn't know it (after a compaction, until the next reply).
    pub context_tokens: Option<u64>,
}
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
    #[serde(default, deserialize_with = "deserialize_optional_model")]
    pub model: Option<PiModel>,

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

/// pi's `Model` object, cut to what ATM reads: `model_select` sends
/// `{id, provider, contextWindow, …}`, and pi-atm's envelope snapshot
/// `{id, provider, contextWindow}`. Captured: pi 0.73.1,
/// `tests/fixtures/pi_tool_run_0.73.1.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiModel {
    pub id: String,
    pub provider: Option<String>,
    /// `contextWindow`, in tokens.
    pub context_window: Option<u32>,
}

fn deserialize_optional_stringish<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(string_from_value))
}

/// Accepts pi's `Model` object or a bare id, with the stringish rules
/// for each part. No id means no model. A `contextWindow` that isn't a
/// JSON integer in `u32` range is dropped (JS writes whole numbers as
/// integers). A 0 gets through; atm-core ignores it.
fn deserialize_optional_model<'de, D>(deserializer: D) -> Result<Option<PiModel>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.and_then(|mut value| {
        let provider = value
            .get_mut("provider")
            .map(Value::take)
            .and_then(string_from_value);
        let context_window = value
            .get("contextWindow")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok());
        string_from_value(value).map(|id| PiModel {
            id,
            provider,
            context_window,
        })
    }))
}

/// Accepts pi-atm's usage snapshot. Anything but an object means no
/// snapshot, and a field that isn't a JSON number of the right kind is
/// dropped, so a bad snapshot never fails the frame.
fn deserialize_optional_usage<'de, D>(deserializer: D) -> Result<Option<PiUsage>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.filter(Value::is_object).map(|value| PiUsage {
        cost_usd: value.get("cost").and_then(Value::as_f64),
        tokens: value.get("tokens").and_then(Value::as_u64),
        context_tokens: value.get("contextTokens").and_then(Value::as_u64),
    }))
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

    // The `parse_*` payloads below follow the spike trace.
    // Captured: pi, version not recorded, 2026-05-02 spike trace (see
    // the crate docs). Only the session_start payload was recorded as a
    // line from that trace; the others name no source. pi 0.73.1 sends
    // the same fields: `tests/fixtures/pi_tool_run_0.73.1.jsonl`, plus
    // the tool_execution_start and tool_result lines pi-atm doesn't
    // forward, from the same run's pi-spike tracer (quoted in PR #73).

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
        // tool_call was undeclared in pi's TypeScript types at the spike;
        // pi 0.73.1 declares it.
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
        // Synthetic: shape rebuilt from the atmd log error
        // `Parse error: invalid type: map, expected a string` from pi-atm
        // (#25). pi's real `model` object (0.43.0 to 0.73.1) also carries
        // `provider`; this one has only an id.
        // Some pi extension/runtime fields are object-like even when ATM only
        // needs their stable string id. The adapter should not drop the entire
        // event on that schema drift.
        let json = r#"{
            "event": "model_select",
            "session_id": {"id": "019e3c43-c15a-740c-9a59-8faba2531283"},
            "tmux_pane": {"id": "%28"},
            "payload": {
                "type":"model_select",
                "model": {"id":"claude-sonnet-4-6"}
            }
        }"#;

        let raw: RawPiEvent = serde_json::from_str(json).unwrap();

        assert_eq!(
            raw.session_id.as_deref(),
            Some("019e3c43-c15a-740c-9a59-8faba2531283")
        );
        assert_eq!(raw.tmux_pane.as_deref(), Some("%28"));
        assert_eq!(
            raw.payload.model,
            Some(PiModel {
                id: "claude-sonnet-4-6".into(),
                provider: None,
                context_window: None,
            })
        );
    }

    #[test]
    fn model_without_an_id_is_dropped_instead_of_failing_parse() {
        // Synthetic: a drift case for #25. pi's `Model.id` is a required
        // string, so no release sends these.
        for model in [r#"{"provider": "anthropic"}"#, "[]", "null"] {
            let json = format!(
                r#"{{"event": "model_select", "payload": {{"type": "model_select", "model": {model}}}}}"#
            );

            let raw: RawPiEvent = serde_json::from_str(&json).unwrap();

            assert_eq!(raw.payload.model, None, "model {model}");
        }
    }

    #[test]
    fn model_accepts_a_bare_id() {
        // Synthetic: no pi release sends a string `model`; the stringish
        // field accepted one before `PiModel`, so it still does.
        let json = r#"{
            "event": "model_select",
            "payload": {"type": "model_select", "model": "gpt-5.5"}
        }"#;

        let raw: RawPiEvent = serde_json::from_str(json).unwrap();

        assert_eq!(
            raw.payload.model,
            Some(PiModel {
                id: "gpt-5.5".into(),
                provider: None,
                context_window: None,
            })
        );
    }

    #[test]
    fn envelope_model_snapshot_parses() {
        // Captured: pi 0.73.1, 2026-10-24, the envelope `model` of the
        // `context` frames in `tests/fixtures/pi_tool_run_0.73.1.jsonl`
        // (payload cut). Older pi-atm sends no snapshot.
        let json = r#"{
            "event": "context",
            "payload": {"type": "context", "messages": []},
            "model": {"id": "fake-b", "provider": "atmfake", "contextWindow": 32000}
        }"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            raw.current_model,
            Some(PiModel {
                id: "fake-b".into(),
                provider: Some("atmfake".into()),
                context_window: Some(32_000),
            })
        );

        let older = r#"{"event": "context", "payload": {"type": "context"}}"#;
        let raw: RawPiEvent = serde_json::from_str(older).unwrap();
        assert_eq!(raw.current_model, None);
    }

    #[test]
    fn odd_context_window_is_dropped_instead_of_failing_parse() {
        // Synthetic: drift cases for #25. pi's `Model.contextWindow` is a
        // required number in 0.73.1, so no release sends these.
        for window in ["null", r#""32000""#, "-1", "1.5", "4294967296", "{}"] {
            let json = format!(
                r#"{{"event": "context", "model": {{"id": "fake-b", "contextWindow": {window}}}}}"#
            );

            let raw: RawPiEvent = serde_json::from_str(&json).unwrap();

            let model = raw.current_model.expect("the id still makes a model");
            assert_eq!(model.context_window, None, "contextWindow {window}");
        }

        let json = r#"{"event": "context", "model": {"id": "fake-b"}}"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.current_model.and_then(|m| m.context_window), None);
    }

    #[test]
    fn odd_usage_fields_are_dropped_instead_of_failing_parse() {
        // Synthetic: drift cases. pi-atm sends JS numbers, and
        // `contextTokens: null` when pi doesn't know the context size.
        let json = r#"{"event": "turn_end",
            "usage": {"cost": "0.01", "tokens": -1, "contextTokens": null}}"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(raw.usage, Some(PiUsage::default()));

        let json = r#"{"event": "turn_end",
            "usage": {"cost": 0.01065, "tokens": 3430, "contextTokens": 1205}}"#;
        let raw: RawPiEvent = serde_json::from_str(json).unwrap();
        assert_eq!(
            raw.usage,
            Some(PiUsage {
                cost_usd: Some(0.01065),
                tokens: Some(3430),
                context_tokens: Some(1205),
            })
        );

        for usage in ["null", r#""junk""#, "[]", "1"] {
            let json = format!(r#"{{"event": "turn_end", "usage": {usage}}}"#);
            let raw: RawPiEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(raw.usage, None, "usage {usage}");
        }
    }

    #[test]
    fn stringish_payload_fields_drop_unlabeled_objects_instead_of_failing_parse() {
        // Synthetic: object-valued `text`, a drift case for #25.
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
