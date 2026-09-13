//! Translation from Claude raw `RawHookEvent` to vendor-neutral
//! `LifecycleEvent`.
//!
//! This is the *only* place Claude semantics get mapped to atm-core
//! types. The daemon calls this at the connection boundary and
//! everything downstream sees only `LifecycleEvent`.

use atm_core::{LifecycleEvent, NeedsInputReason, NotificationKind, Tool};

use crate::event::ClaudeEventType;
use crate::wire::RawHookEvent;

const PERMISSION_LABEL_MAX_CHARS: usize = 60;

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl RawHookEvent {
    /// Child id, teammate name, and role for an event emitted by an in-process child.
    pub fn child_agent(&self) -> Option<(Option<String>, Option<String>, Option<String>)> {
        let event = self.event_type()?;
        if matches!(
            event,
            ClaudeEventType::SubagentStart | ClaudeEventType::SubagentStop
        ) {
            return None;
        }
        let id = non_empty(self.agent_id.as_deref());
        let name = matches!(
            event,
            ClaudeEventType::TeammateIdle
                | ClaudeEventType::TaskCreated
                | ClaudeEventType::TaskCompleted
        )
        .then(|| non_empty(self.teammate_name.as_deref()))
        .flatten();
        (id.is_some() || name.is_some()).then(|| {
            let role = self
                .agent_type
                .clone()
                .or_else(|| name.as_ref().map(|_| "teammate".into()));
            (id, name, role)
        })
    }

    /// Name and id reported by a completed named `Agent` spawn.
    pub fn child_alias(&self) -> Option<(String, String)> {
        if self.event_type() != Some(ClaudeEventType::PostToolUse)
            || self.tool_name.as_deref() != Some("Agent")
        {
            return None;
        }
        Some((
            non_empty(self.tool_input.as_ref()?.get("name")?.as_str())?,
            non_empty(self.tool_response.as_ref()?.get("agentId")?.as_str())?,
        ))
    }

    /// Background and scheduled work reported by a parent `Stop`.
    pub fn background_activity(&self) -> Option<(u32, u32)> {
        if self.event_type() != Some(ClaudeEventType::Stop)
            || (self.background_tasks.is_none() && self.session_crons.is_none())
        {
            return None;
        }
        let count = |value: &Option<serde_json::Value>| {
            value
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map_or(0, |items| u32::try_from(items.len()).unwrap_or(u32::MAX))
        };
        Some((count(&self.background_tasks), count(&self.session_crons)))
    }

    fn permission_label(&self) -> Option<String> {
        let tool = non_empty(self.tool_name.as_deref())?;
        let detail = self.tool_input.as_ref().and_then(|input| {
            ["command", "file_path", "path", "url", "description"]
                .iter()
                .find_map(|key| input.get(key)?.as_str())
        });
        Some(detail.map_or(tool.clone(), |detail| {
            let detail = detail.trim();
            let truncated = detail.chars().count() > PERMISSION_LABEL_MAX_CHARS;
            let mut label: String = detail.chars().take(PERMISSION_LABEL_MAX_CHARS).collect();
            if truncated {
                label.push('…');
            }
            format!("{tool}: {label}")
        }))
    }

    /// Translates this Claude raw event into a vendor-neutral
    /// `LifecycleEvent`.
    ///
    /// Returns `None` if `hook_event_name` does not match a known
    /// Claude event. The translation collapses Claude-specific
    /// distinctions where the underlying concept is vendor-neutral
    /// (e.g. `PostToolUse`/`PostToolUseFailure` both become
    /// `ToolCallEnd`, distinguished by `is_error`).
    ///
    /// Carry-through fidelity: `tool_use_id` (Claude `tool_use_id` /
    /// pi `toolCallId`) and tool input rides `ToolCallStart`. `source`
    /// (SessionStart), `reason` (SessionEnd), `trigger` (PreCompact),
    /// and `prompt` (UserPromptSubmit) are preserved on their target
    /// variants.
    pub fn to_lifecycle_event(&self) -> Option<LifecycleEvent> {
        let ev = self.event_type()?;
        // Tool-name presence is required for the three tool-shaped
        // events. A PreToolUse / PostToolUse / PostToolUseFailure
        // without a tool_name is malformed; returning `None` (treat as
        // unknown event) is safer than fabricating `Tool::Other("")`,
        // which would silently inject phantom tool-call records into
        // the registry.
        let needs_tool = matches!(
            ev,
            ClaudeEventType::PreToolUse
                | ClaudeEventType::PostToolUse
                | ClaudeEventType::PostToolUseFailure
        );
        let tool_name = self.tool_name.as_deref().unwrap_or("");
        if needs_tool && tool_name.is_empty() {
            return None;
        }
        let tool = Tool::from(tool_name);
        Some(match ev {
            ClaudeEventType::PreToolUse => {
                if tool.is_interactive() {
                    LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::InteractiveTool { tool },
                    }
                } else {
                    LifecycleEvent::ToolCallStart {
                        name: tool,
                        tool_use_id: self.tool_use_id.clone(),
                        input: self.tool_input.clone(),
                    }
                }
            }
            ClaudeEventType::PostToolUse => LifecycleEvent::ToolCallEnd {
                name: tool,
                tool_use_id: self.tool_use_id.clone(),
                is_error: false,
            },
            ClaudeEventType::PostToolUseFailure => LifecycleEvent::ToolCallEnd {
                name: tool,
                tool_use_id: self.tool_use_id.clone(),
                is_error: true,
            },
            ClaudeEventType::UserPromptSubmit => LifecycleEvent::PromptSubmit {
                prompt: self.prompt.clone(),
            },
            ClaudeEventType::Stop => LifecycleEvent::WorkingEnd,
            ClaudeEventType::SubagentStart => LifecycleEvent::ChildSessionStart {
                id: self.agent_id.clone(),
                role: self.agent_type.clone(),
            },
            ClaudeEventType::SubagentStop => LifecycleEvent::ChildSessionEnd {
                id: self.agent_id.clone(),
            },
            ClaudeEventType::SessionStart => LifecycleEvent::SessionStart {
                source: self.source.clone(),
            },
            ClaudeEventType::SessionEnd => LifecycleEvent::SessionEnd {
                reason: self.reason.clone(),
            },
            ClaudeEventType::PreCompact => LifecycleEvent::ContextCompactStart {
                trigger: self.trigger.clone(),
            },
            ClaudeEventType::Setup => LifecycleEvent::Notification {
                message: None,
                kind: Some(NotificationKind::Setup),
            },
            ClaudeEventType::Notification => {
                let kind = self
                    .notification_type
                    .as_deref()
                    .map(NotificationKind::from);
                match kind {
                    Some(
                        k @ (NotificationKind::PermissionPrompt
                        | NotificationKind::ElicitationDialog),
                    ) => LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::Notification {
                            kind: k,
                            // Claude `Notification` events don't carry
                            // a per-prompt label — only a kind tag.
                            label: None,
                        },
                    },
                    Some(NotificationKind::IdlePrompt) => LifecycleEvent::Idle,
                    Some(k) if k.as_str() == "agent_needs_input" => LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::Notification {
                            kind: k,
                            label: self.message.clone(),
                        },
                    },
                    _ => LifecycleEvent::Notification {
                        message: self.message.clone(),
                        kind,
                    },
                }
            }
            ClaudeEventType::PermissionRequest => LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: self.permission_label(),
                },
            },
            ClaudeEventType::TeammateIdle => {
                self.child_agent()?;
                LifecycleEvent::Idle
            }
            ClaudeEventType::TaskCreated | ClaudeEventType::TaskCompleted => {
                LifecycleEvent::Notification {
                    message: non_empty(self.task_subject.as_deref()),
                    kind: Some(NotificationKind::from(match ev {
                        ClaudeEventType::TaskCreated => "task_created",
                        _ => "task_completed",
                    })),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(name: &str) -> RawHookEvent {
        RawHookEvent {
            session_id: "s".into(),
            hook_event_name: name.into(),
            cwd: None,
            permission_mode: None,
            pid: None,
            tmux_pane: None,
            tool_name: None,
            tool_input: None,
            tool_response: None,
            tool_use_id: None,
            prompt: None,
            stop_hook_active: None,
            background_tasks: None,
            session_crons: None,
            agent_id: None,
            agent_type: None,
            agent_transcript_path: None,
            source: None,
            reason: None,
            model: None,
            trigger: None,
            custom_instructions: None,
            notification_type: None,
            message: None,
            teammate_name: None,
            task_subject: None,
        }
    }

    #[test]
    fn pre_tool_use_non_interactive_carries_tool_use_id_and_input() {
        let mut e = raw("PreToolUse");
        e.tool_name = Some("Bash".into());
        e.tool_use_id = Some("toolu_01abc".into());
        e.tool_input = Some(serde_json::json!({"command": "ls"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: Some("toolu_01abc".into()),
                input: Some(serde_json::json!({"command": "ls"})),
            })
        );
    }

    #[test]
    fn pre_tool_use_unknown_tool_lands_in_other() {
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
    fn pre_tool_use_interactive_becomes_needs_input() {
        for (name, expected) in [
            ("AskUserQuestion", Tool::AskUserQuestion),
            ("EnterPlanMode", Tool::EnterPlanMode),
            ("ExitPlanMode", Tool::ExitPlanMode),
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
        // PreToolUse / PostToolUse / PostToolUseFailure without a
        // tool_name are malformed — translating them used to fabricate
        // Tool::Other("") and inject phantom records into the registry.
        // The guard now drops them; verify across both `None` and
        // empty-string forms.
        for name in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
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

        // Negative: a non-tool event without tool_name still translates
        // (the guard scopes to the three tool-shaped events only).
        assert!(raw("Stop").to_lifecycle_event().is_some());
    }

    #[test]
    fn post_tool_use_distinguishes_failure() {
        let mut ok = raw("PostToolUse");
        ok.tool_name = Some("Bash".into());
        ok.tool_use_id = Some("toolu_xyz".into());
        let mut fail = raw("PostToolUseFailure");
        fail.tool_name = Some("Bash".into());
        fail.tool_use_id = Some("toolu_xyz".into());

        assert_eq!(
            ok.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: Some("toolu_xyz".into()),
                is_error: false,
            })
        );
        assert_eq!(
            fail.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: Some("toolu_xyz".into()),
                is_error: true,
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
        let mut end = raw("SessionEnd");
        end.reason = Some("clear".into());
        assert_eq!(
            end.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("clear".into())
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
    fn setup_to_setup_notification() {
        assert_eq!(
            raw("Setup").to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: None,
                kind: Some(NotificationKind::Setup),
            })
        );
    }

    #[test]
    fn notification_permission_to_needs_input() {
        let mut e = raw("Notification");
        e.notification_type = Some("permission_prompt".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: None,
                }
            })
        );
    }

    #[test]
    fn notification_idle_to_idle() {
        let mut e = raw("Notification");
        e.notification_type = Some("idle_prompt".into());
        assert_eq!(e.to_lifecycle_event(), Some(LifecycleEvent::Idle));
    }

    #[test]
    fn notification_generic_passthrough() {
        let mut e = raw("Notification");
        e.notification_type = Some("info".into());
        e.message = Some("hi".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some("hi".into()),
                kind: Some(NotificationKind::Info),
            })
        );
    }

    #[test]
    fn unknown_event_returns_none() {
        let e = raw("NotARealEvent");
        assert_eq!(e.to_lifecycle_event(), None);
    }

    #[test]
    fn captured_subagent_run_preserves_routing_contract() {
        let events: Vec<RawHookEvent> =
            include_str!("../tests/fixtures/claude_subagent_run_2.1.267.jsonl")
                .lines()
                .map(|line| serde_json::from_str(line).expect("fixture line"))
                .collect();
        assert!(events
            .iter()
            .all(|event| event.to_lifecycle_event().is_some()));
        let routed: Vec<_> = events
            .iter()
            .filter(|event| event.child_agent().is_some())
            .map(|event| event.hook_event_name.as_str())
            .collect();
        assert_eq!(routed, ["PreToolUse", "PostToolUse"]);
        assert!(events
            .iter()
            .any(|event| event.background_activity() == Some((0, 0))));
    }

    #[test]
    fn teammate_metadata_and_alias_are_extracted() {
        let mut idle = raw("TeammateIdle");
        idle.teammate_name = Some("reviewer".into());
        assert_eq!(idle.to_lifecycle_event(), Some(LifecycleEvent::Idle));
        assert_eq!(
            idle.child_agent(),
            Some((None, Some("reviewer".into()), Some("teammate".into())))
        );

        let mut spawn = raw("PostToolUse");
        spawn.tool_name = Some("Agent".into());
        spawn.tool_input = Some(serde_json::json!({"name": "reviewer"}));
        spawn.tool_response = Some(serde_json::json!({"agentId": "agent-1"}));
        assert_eq!(
            spawn.child_alias(),
            Some(("reviewer".into(), "agent-1".into()))
        );
    }

    #[test]
    fn new_orchestration_signals_translate() {
        let mut permission = raw("PermissionRequest");
        permission.tool_name = Some("Bash".into());
        permission.tool_input = Some(serde_json::json!({"command": "cargo test"}));
        assert!(matches!(
            permission.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    label: Some(label), ..
                }
            }) if label == "Bash: cargo test"
        ));

        let mut notification = raw("Notification");
        notification.notification_type = Some("agent_needs_input".into());
        notification.message = Some("reviewer needs input".into());
        assert!(matches!(
            notification.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput { .. })
        ));

        let mut task = raw("TaskCompleted");
        task.task_subject = Some("Review changes".into());
        assert!(matches!(
            task.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some(message), ..
            }) if message == "Review changes"
        ));
    }
}
