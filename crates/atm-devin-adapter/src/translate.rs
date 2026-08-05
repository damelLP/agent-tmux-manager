//! Translation from Devin CLI's raw `RawDevinHookEvent` to
//! vendor-neutral `LifecycleEvent`.
//!
//! This is the *only* place Devin CLI semantics get mapped to
//! `atm-core` types. The daemon calls this at the connection boundary
//! and everything downstream sees only `LifecycleEvent`.

use atm_core::{LifecycleEvent, NeedsInputReason, NotificationKind, Tool};

use crate::event::DevinEventType;
use crate::wire::RawDevinHookEvent;

/// Maps a Devin CLI tool name onto atm-core's `Tool` enum.
///
/// Devin's tool names are lowercase/`snake_case` (e.g. `exec`, `edit`,
/// `todo_write`) rather than Claude's PascalCase (`Bash`, `Edit`,
/// `TodoWrite`). `Tool::from` only recognizes Claude's exact casing,
/// so tool-name normalization for a vendor with different casing
/// lives here rather than in the shared `Tool` type — keeping vendor
/// knowledge contained to its own adapter crate, per this crate's
/// stated design.
///
/// Devin tool names without a clear Claude-shaped equivalent (e.g.
/// `apply_patch`, `skill`, `mcp_*`) fall through to `Tool::Other`,
/// preserving Devin's native name.
fn normalize_tool_name(name: &str) -> Tool {
    match name {
        "read" => Tool::Read,
        "write" => Tool::Write,
        "edit" => Tool::Edit,
        "grep" => Tool::Grep,
        "glob" => Tool::Glob,
        "exec" => Tool::Bash,
        "webfetch" => Tool::WebFetch,
        "todo_write" => Tool::TodoWrite,
        "notebook_edit" => Tool::NotebookEdit,
        "notebook_read" => Tool::NotebookRead,
        "exit_plan_mode" => Tool::ExitPlanMode,
        "ask_user_question" => Tool::AskUserQuestion,
        "run_subagent" => Tool::Task,
        other => Tool::Other(other.to_string()),
    }
}

impl RawDevinHookEvent {
    /// Translates this Devin CLI raw event into a vendor-neutral
    /// `LifecycleEvent`.
    ///
    /// Returns `None` if `hook_event_name` does not match a known
    /// Devin event, or if a tool-shaped event (`PreToolUse`,
    /// `PostToolUse`, `PermissionRequest`) is missing `tool_name`
    /// (malformed — mirrors `atm_claude_adapter`'s guard against
    /// fabricating phantom tool-call records, and keeps
    /// `PermissionRequest` consistent since it also relies on
    /// `tool_name` for its `NeedsInput` label).
    pub fn to_lifecycle_event(&self) -> Option<LifecycleEvent> {
        let ev = self.event_type()?;
        let needs_tool = matches!(
            ev,
            DevinEventType::PreToolUse
                | DevinEventType::PostToolUse
                | DevinEventType::PermissionRequest
        );
        let tool_name = self.tool_name.as_deref().unwrap_or("");
        if needs_tool && tool_name.is_empty() {
            return None;
        }
        let tool = normalize_tool_name(tool_name);
        Some(match ev {
            DevinEventType::PreToolUse => {
                if tool.is_interactive() {
                    LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::InteractiveTool { tool },
                    }
                } else {
                    LifecycleEvent::ToolCallStart {
                        name: tool,
                        tool_use_id: None,
                        input: self.tool_input.clone(),
                    }
                }
            }
            DevinEventType::PostToolUse => LifecycleEvent::ToolCallEnd {
                name: tool,
                tool_use_id: None,
                is_error: self.tool_failed(),
            },
            DevinEventType::PermissionRequest => LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: self.tool_name.clone(),
                },
            },
            DevinEventType::UserPromptSubmit => LifecycleEvent::PromptSubmit {
                prompt: self.prompt.clone(),
            },
            DevinEventType::Stop => LifecycleEvent::WorkingEnd,
            DevinEventType::PostCompaction => LifecycleEvent::Notification {
                message: self.summary.clone(),
                kind: Some(NotificationKind::Info),
            },
            DevinEventType::SessionStart => LifecycleEvent::SessionStart {
                source: self.source.clone(),
            },
            DevinEventType::SessionEnd => LifecycleEvent::SessionEnd {
                reason: self.reason.clone(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawDevinHookEvent {
        RawDevinHookEvent {
            session_id: "s".into(),
            hook_event_name: name.into(),
            prompt_id: None,
            pid: None,
            tmux_pane: None,
            tool_name: None,
            tool_input: None,
            tool_response: None,
            prompt: None,
            stop_hook_active: None,
            summary: None,
            source: None,
            reason: None,
        }
    }

    #[test]
    fn pre_tool_use_maps_known_tool_and_carries_input() {
        let mut e = raw("PreToolUse");
        e.tool_name = Some("exec".into());
        e.tool_input = Some(serde_json::json!({"command": "ls"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: None,
                input: Some(serde_json::json!({"command": "ls"})),
            })
        );
    }

    #[test]
    fn pre_tool_use_unmapped_tool_lands_in_other() {
        let mut e = raw("PreToolUse");
        e.tool_name = Some("mcp__github__list_issues".into());
        match e.to_lifecycle_event() {
            Some(LifecycleEvent::ToolCallStart { name, .. }) => {
                assert_eq!(name, Tool::Other("mcp__github__list_issues".into()));
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn pre_tool_use_interactive_tools_become_needs_input() {
        for (name, expected) in [
            ("exit_plan_mode", Tool::ExitPlanMode),
            ("ask_user_question", Tool::AskUserQuestion),
        ] {
            let mut e = raw("PreToolUse");
            e.tool_name = Some(name.into());
            assert_eq!(
                e.to_lifecycle_event(),
                Some(LifecycleEvent::NeedsInput {
                    reason: NeedsInputReason::InteractiveTool { tool: expected }
                }),
                "tool {name} should map to NeedsInput"
            );
        }
    }

    #[test]
    fn tool_shaped_event_without_tool_name_returns_none() {
        for name in ["PreToolUse", "PostToolUse", "PermissionRequest"] {
            assert_eq!(raw(name).to_lifecycle_event(), None);
            let mut empty = raw(name);
            empty.tool_name = Some(String::new());
            assert_eq!(empty.to_lifecycle_event(), None);
        }
    }

    #[test]
    fn post_tool_use_distinguishes_failure_via_tool_response() {
        let mut ok = raw("PostToolUse");
        ok.tool_name = Some("exec".into());
        ok.tool_response = Some(serde_json::json!({"success": true, "output": "", "error": null}));
        assert_eq!(
            ok.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: None,
                is_error: false,
            })
        );

        let mut fail = raw("PostToolUse");
        fail.tool_name = Some("exec".into());
        fail.tool_response =
            Some(serde_json::json!({"success": false, "output": "", "error": "boom"}));
        assert_eq!(
            fail.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: None,
                is_error: true,
            })
        );
    }

    #[test]
    fn permission_request_becomes_needs_input_with_tool_label() {
        let mut e = raw("PermissionRequest");
        e.tool_name = Some("exec".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: Some("exec".into()),
                }
            })
        );
    }

    #[test]
    fn user_prompt_carries_prompt() {
        let mut e = raw("UserPromptSubmit");
        e.prompt = Some("fix the tests".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::PromptSubmit {
                prompt: Some("fix the tests".into())
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
    fn post_compaction_to_notification() {
        let mut e = raw("PostCompaction");
        e.summary = Some("Compacted 40 messages".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some("Compacted 40 messages".into()),
                kind: Some(NotificationKind::Info),
            })
        );
    }

    #[test]
    fn session_start_carries_source() {
        let mut e = raw("SessionStart");
        e.source = Some("resume".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("resume".into())
            })
        );
    }

    #[test]
    fn session_end_carries_reason() {
        let mut e = raw("SessionEnd");
        e.reason = Some("user_exit".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("user_exit".into())
            })
        );
    }

    #[test]
    fn unknown_event_returns_none() {
        assert_eq!(raw("NotARealEvent").to_lifecycle_event(), None);
    }
}
