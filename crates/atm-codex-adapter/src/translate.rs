//! Translation from Codex raw `RawCodexEvent` to vendor-neutral
//! `LifecycleEvent`.
//!
//! This is the *only* place Codex semantics get mapped to atm-core
//! types. The daemon calls this at the connection boundary and
//! everything downstream sees only `LifecycleEvent`.
//!
//! ## Mapping table (validated against codex-cli 0.146.1, 2026-08-10 spike)
//!
//! | Codex event         | LifecycleEvent                                  |
//! |---------------------|-------------------------------------------------|
//! | `SessionStart`      | `SessionStart { source }`                       |
//! | `SessionEnd`        | `SessionEnd { reason }`                         |
//! | `UserPromptSubmit`  | `PromptSubmit { prompt }`                       |
//! | `PreToolUse`        | `ToolCallStart { name, tool_use_id, input }`    |
//! | `PostToolUse`       | `ToolCallEnd { name, tool_use_id, is_error: false }` |
//! | `PermissionRequest` | `NeedsInput { PermissionGate { tool } }` (or generic `NeedsInput` without a tool) |
//! | `PreCompact`        | `ContextCompactStart { trigger }`               |
//! | `PostCompact`       | `Notification { kind: Other("post_compact") }`  |
//! | `Stop`              | `WorkingEnd`                                    |
//! | `SubagentStart`     | `ChildSessionStart { id, role }`                |
//! | `SubagentStop`      | `ChildSessionEnd { id }`                        |
//! | unknown             | `None` (suppressed)                             |
//!
//! ## Deliberate divergences from the Claude adapter
//!
//! - **No `Tool::is_interactive()` check on `PreToolUse`.** Claude has
//!   no permission event, so its adapter infers blocking from an
//!   allowlist of interactive tool names. Codex signals blocking
//!   explicitly via `PermissionRequest`, so we trust that signal
//!   instead of guessing at Codex tool names the allowlist was never
//!   designed for.
//! - **`is_error` is always `false` on `ToolCallEnd`.** Codex has no
//!   `PostToolUseFailure` event and its `tool_response` is a plain
//!   output string with no error marker (spike-verified against a
//!   genuinely failing tool call). Never fabricate a failure.
//! - **`PermissionRequest` is never dropped.** A missing `tool_name`
//!   degrades to a generic `NeedsInput` notification instead of `None`:
//!   dropping it would leave a genuinely-blocked session showing
//!   "working" forever — a worse failure mode than an unlabelled
//!   needs-input state.

use atm_core::{LifecycleEvent, NeedsInputReason, NotificationKind, Tool};

use crate::event::CodexEventType;
use crate::wire::RawCodexEvent;

impl RawCodexEvent {
    /// Translates this Codex raw event into a vendor-neutral
    /// `LifecycleEvent`.
    ///
    /// Returns `None` if `hook_event_name` does not match a known
    /// Codex event, or if a tool-shaped event (`PreToolUse` /
    /// `PostToolUse`) arrives without a `tool_name` — translating
    /// those would fabricate `Tool::Other("")` and inject phantom
    /// tool-call records into the registry.
    pub fn to_lifecycle_event(&self) -> Option<LifecycleEvent> {
        let ev = self.event_type()?;
        // Tool-name presence is required for the two tool-execution
        // events. PermissionRequest is deliberately NOT in this guard —
        // see the module doc's "never dropped" rationale.
        let needs_tool = matches!(ev, CodexEventType::PreToolUse | CodexEventType::PostToolUse);
        let tool_name = self.tool_name.as_deref().unwrap_or("");
        if needs_tool && tool_name.is_empty() {
            return None;
        }
        Some(match ev {
            CodexEventType::PreToolUse => LifecycleEvent::ToolCallStart {
                name: Tool::from(tool_name),
                tool_use_id: self.tool_use_id.clone(),
                input: self.tool_input.clone(),
            },
            CodexEventType::PostToolUse => LifecycleEvent::ToolCallEnd {
                name: Tool::from(tool_name),
                tool_use_id: self.tool_use_id.clone(),
                // Codex exposes no error signal on PostToolUse.
                is_error: false,
            },
            CodexEventType::PermissionRequest => {
                let reason = if tool_name.is_empty() {
                    NeedsInputReason::Notification {
                        kind: NotificationKind::PermissionPrompt,
                        label: None,
                    }
                } else {
                    NeedsInputReason::PermissionGate {
                        tool: Tool::from(tool_name),
                    }
                };
                LifecycleEvent::NeedsInput { reason }
            }
            CodexEventType::UserPromptSubmit => LifecycleEvent::PromptSubmit {
                prompt: self.prompt.clone(),
            },
            CodexEventType::Stop => LifecycleEvent::WorkingEnd,
            CodexEventType::SubagentStart => LifecycleEvent::ChildSessionStart {
                id: self.agent_id.clone(),
                role: self.agent_type.clone(),
            },
            CodexEventType::SubagentStop => LifecycleEvent::ChildSessionEnd {
                id: self.agent_id.clone(),
            },
            CodexEventType::SessionStart => LifecycleEvent::SessionStart {
                source: self.source.clone(),
            },
            CodexEventType::SessionEnd => LifecycleEvent::SessionEnd {
                reason: self.reason.clone(),
            },
            CodexEventType::PreCompact => LifecycleEvent::ContextCompactStart {
                trigger: self.trigger.clone(),
            },
            // Codex-only concept with no first-class LifecycleEvent
            // variant; ride the open NotificationKind tail (mirrors
            // Claude's Setup → Notification(Setup) precedent) rather
            // than growing atm-core for a single-vendor signal.
            CodexEventType::PostCompact => LifecycleEvent::Notification {
                message: None,
                kind: Some(NotificationKind::from("post_compact")),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawCodexEvent {
        RawCodexEvent {
            session_id: "s".into(),
            hook_event_name: name.into(),
            cwd: None,
            transcript_path: None,
            model: None,
            permission_mode: None,
            turn_id: None,
            pid: None,
            tmux_pane: None,
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_use_id: None,
            prompt: None,
            stop_hook_active: None,
            last_assistant_message: None,
            agent_id: None,
            agent_type: None,
            agent_transcript_path: None,
            source: None,
            reason: None,
            trigger: None,
        }
    }

    #[test]
    fn pre_tool_use_carries_tool_use_id_and_input() {
        let mut e = raw("PreToolUse");
        e.tool_name = Some("Bash".into());
        e.tool_use_id = Some("exec-0faab0a6".into());
        e.tool_input = Some(serde_json::json!({"command": "echo hi"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: Some("exec-0faab0a6".into()),
                input: Some(serde_json::json!({"command": "echo hi"})),
            })
        );
    }

    #[test]
    fn pre_tool_use_unknown_tool_lands_in_other() {
        // Codex tool names outside the shared canonical set (e.g. its
        // snake_case "apply_patch", observed in the spike) ride
        // Tool::Other verbatim.
        let mut e = raw("PreToolUse");
        e.tool_name = Some("apply_patch".into());
        match e.to_lifecycle_event() {
            Some(LifecycleEvent::ToolCallStart { name, .. }) => {
                assert_eq!(name, Tool::Other("apply_patch".into()));
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn tool_shaped_event_without_tool_name_returns_none() {
        for name in ["PreToolUse", "PostToolUse"] {
            assert_eq!(
                raw(name).to_lifecycle_event(),
                None,
                "{name} with tool_name=None should drop"
            );
            let mut empty = raw(name);
            empty.tool_name = Some(String::new());
            assert_eq!(
                empty.to_lifecycle_event(),
                None,
                "{name} with empty tool_name should drop"
            );
        }

        // Negative: a non-tool event without tool_name still translates.
        assert!(raw("Stop").to_lifecycle_event().is_some());
    }

    #[test]
    fn post_tool_use_is_never_an_error() {
        // Codex has no PostToolUseFailure and no error marker in
        // tool_response (spike-verified) — is_error must stay false
        // even when the response text looks like a failure.
        let mut e = raw("PostToolUse");
        e.tool_name = Some("Bash".into());
        e.tool_use_id = Some("exec-41774dae".into());
        e.tool_response = Some(serde_json::Value::String(
            "thread 'main' panicked at linux-sandbox: Permission denied".into(),
        ));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: Some("exec-41774dae".into()),
                is_error: false,
            })
        );
    }

    #[test]
    fn permission_request_with_tool_becomes_permission_gate() {
        let mut e = raw("PermissionRequest");
        e.tool_name = Some("Bash".into());
        e.tool_input = Some(serde_json::json!({"command": "touch x"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            })
        );
    }

    #[test]
    fn permission_request_without_tool_degrades_to_generic_needs_input() {
        // Never dropped: a blocked session must surface as NeedsInput
        // even when the payload carries no tool_name.
        assert_eq!(
            raw("PermissionRequest").to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: None,
                },
            })
        );
    }

    #[test]
    fn user_prompt_carries_prompt() {
        let mut e = raw("UserPromptSubmit");
        e.prompt = Some("hello".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::PromptSubmit {
                prompt: Some("hello".into())
            })
        );
    }

    #[test]
    fn stop_to_working_end() {
        assert_eq!(
            raw("Stop").to_lifecycle_event(),
            Some(LifecycleEvent::WorkingEnd)
        );
    }

    #[test]
    fn subagent_to_child_session() {
        let mut start = raw("SubagentStart");
        start.agent_id = Some("a-1".into());
        start.agent_type = Some("explore".into());
        assert_eq!(
            start.to_lifecycle_event(),
            Some(LifecycleEvent::ChildSessionStart {
                id: Some("a-1".into()),
                role: Some("explore".into()),
            })
        );

        let mut stop = raw("SubagentStop");
        stop.agent_id = Some("a-1".into());
        assert_eq!(
            stop.to_lifecycle_event(),
            Some(LifecycleEvent::ChildSessionEnd {
                id: Some("a-1".into()),
            })
        );
    }

    #[test]
    fn session_start_carries_source() {
        let mut e = raw("SessionStart");
        e.source = Some("startup".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );
    }

    #[test]
    fn session_end_carries_reason() {
        let mut e = raw("SessionEnd");
        e.reason = Some("other".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("other".into())
            })
        );
    }

    #[test]
    fn pre_compact_carries_trigger() {
        let mut e = raw("PreCompact");
        e.trigger = Some("auto".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextCompactStart {
                trigger: Some("auto".into())
            })
        );
    }

    #[test]
    fn post_compact_to_notification() {
        assert_eq!(
            raw("PostCompact").to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: None,
                kind: Some(NotificationKind::Other("post_compact".into())),
            })
        );
    }

    #[test]
    fn unknown_event_returns_none() {
        assert_eq!(raw("NotARealEvent").to_lifecycle_event(), None);
    }
}

/// FEATURE PARITY with the Claude adapter.
///
/// Codex and Claude have deliberately different raw vocabularies but
/// must drive the same downstream session state. These tests feed
/// equivalent wire payloads through both adapters and compare the
/// resulting `LifecycleEvent`s — including one *intentional*
/// divergence that must not be "fixed" into false consistency.
#[cfg(test)]
mod parity_tests {
    use super::*;

    fn codex_raw(json: &str) -> RawCodexEvent {
        serde_json::from_str(json).expect("valid codex wire JSON")
    }

    fn claude_raw(json: &str) -> atm_claude_adapter::RawHookEvent {
        serde_json::from_str(json).expect("valid claude wire JSON")
    }

    #[test]
    fn parity_ordinary_tool_call_produces_same_shape() {
        let codex = codex_raw(
            r#"{"session_id":"s","hook_event_name":"PreToolUse",
                "tool_name":"Bash","tool_use_id":"id-1",
                "tool_input":{"command":"ls"}}"#,
        );
        let claude = claude_raw(
            r#"{"session_id":"s","hook_event_name":"PreToolUse",
                "tool_name":"Bash","tool_use_id":"id-1",
                "tool_input":{"command":"ls"}}"#,
        );
        assert_eq!(
            codex.to_lifecycle_event(),
            claude.to_lifecycle_event(),
            "equivalent tool calls must translate identically"
        );
    }

    #[test]
    fn parity_session_lifecycle_matches_claude_shape() {
        let codex =
            codex_raw(r#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"other"}"#);
        let claude =
            claude_raw(r#"{"session_id":"s","hook_event_name":"SessionEnd","reason":"other"}"#);
        assert_eq!(codex.to_lifecycle_event(), claude.to_lifecycle_event());
    }

    #[test]
    fn intentional_divergence_needs_input_reasons_differ() {
        // Claude infers blocking from an interactive-tool allowlist
        // (`InteractiveTool`); Codex signals it explicitly via a
        // dedicated event (`PermissionGate`). Both are NeedsInput, and
        // the reasons are deliberately different — this is a
        // regression guard against collapsing them.
        let codex = codex_raw(
            r#"{"session_id":"s","hook_event_name":"PermissionRequest",
                "tool_name":"Bash"}"#,
        );
        let claude = claude_raw(
            r#"{"session_id":"s","hook_event_name":"PreToolUse",
                "tool_name":"AskUserQuestion"}"#,
        );

        let codex_event = codex.to_lifecycle_event();
        let claude_event = claude.to_lifecycle_event();

        assert!(matches!(
            codex_event,
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { .. }
            })
        ));
        assert!(matches!(
            claude_event,
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::InteractiveTool { .. }
            })
        ));
        assert_ne!(
            codex_event, claude_event,
            "the NeedsInput reasons are intentionally distinct per vendor"
        );
    }
}
