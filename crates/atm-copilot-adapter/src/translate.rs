//! Translation from Copilot CLI's raw `RawCopilotHookEvent` to
//! vendor-neutral `LifecycleEvent`.
//!
//! This is the *only* place Copilot CLI semantics get mapped to
//! `atm-core` types. The daemon calls this at the connection boundary
//! and everything downstream sees only `LifecycleEvent`.
//!
//! Copilot's documented hook set doesn't include a per-turn "stop"
//! event (unlike Claude's `Stop` / Devin's `Stop`), so this adapter
//! does not emit `LifecycleEvent::WorkingEnd` — sessions rely on
//! `SessionEnd` for the "no longer active" signal. Revisit if GitHub
//! documents an equivalent (their docs reference an "agent stop"
//! lifecycle point without giving its `hooks.json` event key).

use atm_core::{LifecycleEvent, NeedsInputReason, NotificationKind, Tool};

use crate::event::CopilotEventType;
use crate::wire::RawCopilotHookEvent;

impl RawCopilotHookEvent {
    /// Translates this Copilot CLI raw event into a vendor-neutral
    /// `LifecycleEvent`.
    ///
    /// Returns `None` if `hook_event_name` does not match a known
    /// Copilot event, or if a tool-shaped event (`preToolUse`,
    /// `postToolUse`, `postToolUseFailure`, `permissionRequest`) is
    /// missing `tool_name` (malformed — mirrors the other adapters'
    /// guard against fabricating phantom tool-call records, and keeps
    /// `permissionRequest` consistent since it also relies on
    /// `tool_name` for its `NeedsInput` label).
    pub fn to_lifecycle_event(&self) -> Option<LifecycleEvent> {
        let ev = self.event_type()?;
        let needs_tool = matches!(
            ev,
            CopilotEventType::PreToolUse
                | CopilotEventType::PostToolUse
                | CopilotEventType::PostToolUseFailure
                | CopilotEventType::PermissionRequest
        );
        let tool_name = self.tool_name.as_deref().unwrap_or("");
        if needs_tool && tool_name.is_empty() {
            return None;
        }
        // Copilot's tool-name vocabulary isn't well-documented publicly
        // and its casing doesn't match Claude's, so — unlike
        // `atm-devin-adapter` — no vendor-specific normalization table
        // is asserted here. Unknown names land in `Tool::Other`, which
        // still round-trips the original name for display.
        let tool = Tool::from(tool_name);
        Some(match ev {
            CopilotEventType::PreToolUse => {
                if tool.is_interactive() {
                    LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::InteractiveTool { tool },
                    }
                } else {
                    LifecycleEvent::ToolCallStart {
                        name: tool,
                        tool_use_id: None,
                        input: self.tool_args.clone(),
                    }
                }
            }
            CopilotEventType::PostToolUse => LifecycleEvent::ToolCallEnd {
                name: tool,
                tool_use_id: None,
                is_error: false,
            },
            CopilotEventType::PostToolUseFailure => LifecycleEvent::ToolCallEnd {
                name: tool,
                tool_use_id: None,
                is_error: true,
            },
            CopilotEventType::PermissionRequest => LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: self.tool_name.clone(),
                },
            },
            CopilotEventType::UserPromptSubmitted => LifecycleEvent::PromptSubmit {
                prompt: self.prompt.clone(),
            },
            CopilotEventType::SessionStart => LifecycleEvent::SessionStart {
                source: self.source.clone(),
            },
            CopilotEventType::SessionEnd => LifecycleEvent::SessionEnd {
                reason: self.reason.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawCopilotHookEvent {
        RawCopilotHookEvent {
            session_id: "s".into(),
            hook_event_name: name.into(),
            pid: None,
            tmux_pane: None,
            tool_name: None,
            tool_args: None,
            tool_result: None,
            error: None,
            prompt: None,
            source: None,
            reason: None,
        }
    }

    #[test]
    fn pre_tool_use_carries_args() {
        let mut e = raw("preToolUse");
        e.tool_name = Some("bash".into());
        e.tool_args = Some(serde_json::json!({"command": "ls"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallStart {
                name: Tool::Other("bash".into()),
                tool_use_id: None,
                input: Some(serde_json::json!({"command": "ls"})),
            })
        );
    }

    #[test]
    fn tool_shaped_events_without_tool_name_return_none() {
        for name in [
            "preToolUse",
            "postToolUse",
            "postToolUseFailure",
            "permissionRequest",
        ] {
            assert_eq!(raw(name).to_lifecycle_event(), None);
        }
    }

    #[test]
    fn post_tool_use_and_failure_variant() {
        let mut ok = raw("postToolUse");
        ok.tool_name = Some("bash".into());
        assert_eq!(
            ok.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Other("bash".into()),
                tool_use_id: None,
                is_error: false,
            })
        );

        let mut fail = raw("postToolUseFailure");
        fail.tool_name = Some("bash".into());
        fail.error = Some("boom".into());
        assert_eq!(
            fail.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Other("bash".into()),
                tool_use_id: None,
                is_error: true,
            })
        );
    }

    #[test]
    fn permission_request_becomes_needs_input() {
        let mut e = raw("permissionRequest");
        e.tool_name = Some("bash".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: Some("bash".into()),
                }
            })
        );
    }

    #[test]
    fn user_prompt_submitted_carries_prompt() {
        let mut e = raw("userPromptSubmitted");
        e.prompt = Some("fix the tests".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::PromptSubmit {
                prompt: Some("fix the tests".into())
            })
        );
    }

    #[test]
    fn session_start_and_end_carry_fields() {
        let mut start = raw("sessionStart");
        start.source = Some("startup".into());
        assert_eq!(
            start.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );

        let mut end = raw("sessionEnd");
        end.reason = Some("user_exit".into());
        assert_eq!(
            end.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("user_exit".into())
            })
        );
    }

    #[test]
    fn unknown_event_returns_none() {
        assert_eq!(raw("notARealEvent").to_lifecycle_event(), None);
    }
}
