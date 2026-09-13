//! Translation from Claude raw `RawHookEvent` to vendor-neutral
//! `LifecycleEvent`.
//!
//! This is the *only* place Claude semantics get mapped to atm-core
//! types. The daemon calls this at the connection boundary and
//! everything downstream sees only `LifecycleEvent`.

use atm_core::{ChildAgentRef, LifecycleEvent, NeedsInputReason, NotificationKind, Tool};

use crate::event::ClaudeEventType;
use crate::wire::RawHookEvent;

/// Longest argument excerpt shown in a permission label.
const PERMISSION_LABEL_MAX_CHARS: usize = 60;

/// Role recorded for a child known only by its teammate name.
const TEAMMATE_ROLE: &str = "teammate";

/// Trimmed, non-empty copy of an optional string field.
fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Truncates on a char boundary, appending an ellipsis when cut.
fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

impl RawHookEvent {
    /// The in-process child agent this event was fired from or is
    /// about, if any.
    ///
    /// Claude tags every hook fired inside a subagent or teammate with
    /// `agent_id` / `agent_type` while keeping the parent's
    /// `session_id`. `SubagentStart` / `SubagentStop` also carry the id
    /// but are *about* the child rather than *from* it, so they are
    /// excluded here and handled through `ChildSessionStart` / `End`.
    ///
    /// `TeammateIdle`, `TaskCreated` and `TaskCompleted` identify the
    /// teammate by `teammate_name` instead (Claude Code 2.1.267 hook
    /// schema). The name becomes the reference and the registry maps it
    /// to the agent id through the alias recorded from the spawning
    /// `Agent` call (see [`Self::child_alias`]).
    pub fn child_agent(&self) -> Option<ChildAgentRef> {
        let ev = self.event_type()?;
        let id = non_empty(self.agent_id.as_deref());
        match ev {
            ClaudeEventType::SubagentStart | ClaudeEventType::SubagentStop => None,
            ClaudeEventType::TeammateIdle
            | ClaudeEventType::TaskCreated
            | ClaudeEventType::TaskCompleted => {
                let name = non_empty(self.teammate_name.as_deref());
                if id.is_none() && name.is_none() {
                    return None;
                }
                let role = self
                    .agent_type
                    .clone()
                    .or_else(|| Some(TEAMMATE_ROLE.to_string()));
                Some(ChildAgentRef { id, name, role })
            }
            _ => id.map(|id| ChildAgentRef {
                id: Some(id),
                name: None,
                role: self.agent_type.clone(),
            }),
        }
    }

    /// `(name, agent_id)` for a named `Agent` spawn, pairing the call's
    /// `name` argument with the `agentId` in its response. Lets the
    /// registry route teammate events that only carry a name.
    pub fn child_alias(&self) -> Option<(String, String)> {
        if self.event_type() != Some(ClaudeEventType::PostToolUse)
            || Tool::from(self.tool_name.as_deref().unwrap_or("")) != Tool::Agent
        {
            return None;
        }
        let name = non_empty(self.tool_input.as_ref()?.get("name")?.as_str())?;
        let agent_id = non_empty(self.tool_response.as_ref()?.get("agentId")?.as_str())?;
        Some((name, agent_id))
    }

    /// What the agent left running when its turn ended, from the
    /// `background_tasks` / `session_crons` arrays on `Stop`. `None` for
    /// other events, or for Claude versions that predate those fields.
    pub fn background_activity(&self) -> Option<LifecycleEvent> {
        if self.event_type() != Some(ClaudeEventType::Stop) {
            return None;
        }
        if self.background_tasks.is_none() && self.session_crons.is_none() {
            return None;
        }
        let count = |value: &Option<serde_json::Value>| -> u32 {
            value
                .as_ref()
                .and_then(serde_json::Value::as_array)
                .map_or(0, |items| u32::try_from(items.len()).unwrap_or(u32::MAX))
        };
        Some(LifecycleEvent::BackgroundActivity {
            running_tasks: count(&self.background_tasks),
            scheduled_tasks: count(&self.session_crons),
        })
    }

    /// Human label for a permission prompt: the tool name plus its most
    /// telling argument (Bash command, file path, URL), truncated.
    fn permission_label(&self) -> Option<String> {
        let tool = self.tool_name.as_deref().filter(|t| !t.is_empty())?;
        let detail = self.tool_input.as_ref().and_then(|input| {
            ["command", "file_path", "path", "url", "description"]
                .iter()
                .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
        });
        Some(match detail {
            Some(d) => format!(
                "{tool}: {}",
                truncate_chars(d.trim(), PERMISSION_LABEL_MAX_CHARS)
            ),
            None => tool.to_string(),
        })
    }

    /// Title of the agent-team task on `TaskCreated` / `TaskCompleted`
    /// (`task_subject`, emitted top-level by Claude Code 2.1.267).
    fn task_subject(&self) -> Option<String> {
        non_empty(self.task_subject.as_deref())
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
                // Claude Code 2.1.267 emits no stop reason on SubagentStop
                // (see tests/fixtures); the field stays reserved for
                // vendors that do.
                reason: None,
                last_message: self.last_assistant_message.clone(),
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
                    // A background agent or teammate is waiting on the
                    // user; Claude's message names which one.
                    Some(k @ NotificationKind::AgentNeedsInput) => LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::Notification {
                            kind: k,
                            label: self.message.clone(),
                        },
                    },
                    Some(NotificationKind::IdlePrompt) => LifecycleEvent::Idle,
                    _ => LifecycleEvent::Notification {
                        message: self.message.clone(),
                        kind,
                    },
                }
            }
            // Fires the moment a tool needs a decision, ~6s before the
            // `permission_prompt` notification, and names the tool.
            ClaudeEventType::PermissionRequest => LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: self.permission_label(),
                },
            },
            // Names the teammate; routed to its session by the registry.
            // Without a resolvable teammate the event is dropped rather
            // than idling the lead session.
            ClaudeEventType::TeammateIdle => {
                self.child_agent()?;
                LifecycleEvent::Idle
            }
            ClaudeEventType::TaskCreated => LifecycleEvent::Notification {
                message: self.task_subject(),
                kind: Some(NotificationKind::TaskCreated),
            },
            ClaudeEventType::TaskCompleted => LifecycleEvent::Notification {
                message: self.task_subject(),
                kind: Some(NotificationKind::TaskCompleted),
            },
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
            last_assistant_message: None,
            teammate_name: None,
            team_name: None,
            task_id: None,
            task_subject: None,
            task_description: None,
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
                reason: None,
                last_message: None,
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
    fn permission_request_becomes_needs_input_with_tool_label() {
        let mut e = raw("PermissionRequest");
        e.tool_name = Some("Bash".into());
        e.tool_input = Some(serde_json::json!({"command": "cargo test --workspace"}));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: Some("Bash: cargo test --workspace".into()),
                },
            })
        );

        // Long arguments are truncated on a char boundary.
        let mut long = raw("PermissionRequest");
        long.tool_name = Some("Bash".into());
        long.tool_input = Some(serde_json::json!({"command": "é".repeat(100)}));
        match long.to_lifecycle_event() {
            Some(LifecycleEvent::NeedsInput {
                reason:
                    NeedsInputReason::Notification {
                        label: Some(label), ..
                    },
            }) => {
                assert!(label.starts_with("Bash: "));
                assert!(label.ends_with('…'));
                assert_eq!(
                    label.chars().count(),
                    "Bash: ".len() + PERMISSION_LABEL_MAX_CHARS + 1
                );
            }
            other => panic!("expected NeedsInput with label, got {other:?}"),
        }

        // No tool name: still a permission wait, just unlabelled.
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
    fn agent_notifications_translate() {
        let mut e = raw("Notification");
        e.notification_type = Some("agent_needs_input".into());
        e.message = Some("Agent reviewer needs your input".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::AgentNeedsInput,
                    label: Some("Agent reviewer needs your input".into()),
                },
            })
        );

        let mut done = raw("Notification");
        done.notification_type = Some("agent_completed".into());
        done.message = Some("Agent reviewer finished".into());
        assert_eq!(
            done.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some("Agent reviewer finished".into()),
                kind: Some(NotificationKind::AgentCompleted),
            })
        );
    }

    #[test]
    fn teammate_and_task_events_translate() {
        // Claude Code 2.1.267 names the teammate; there is no agent_id.
        let mut idle = raw("TeammateIdle");
        idle.teammate_name = Some("reviewer".into());
        idle.team_name = Some("main".into());
        assert_eq!(idle.to_lifecycle_event(), Some(LifecycleEvent::Idle));
        assert_eq!(
            idle.child_agent(),
            Some(ChildAgentRef {
                id: None,
                name: Some("reviewer".into()),
                role: Some("teammate".into()),
            })
        );
        // Should a future version add agent_id, both are carried so the
        // registry can record the pairing.
        idle.agent_id = Some("a1b2".into());
        idle.agent_type = Some("worker".into());
        assert_eq!(
            idle.child_agent(),
            Some(ChildAgentRef {
                id: Some("a1b2".into()),
                name: Some("reviewer".into()),
                role: Some("worker".into()),
            })
        );
        // A TeammateIdle that names nobody is dropped, never idling the lead.
        assert_eq!(raw("TeammateIdle").to_lifecycle_event(), None);

        let mut created = raw("TaskCreated");
        created.task_id = Some("t-1".into());
        created.task_subject = Some("Write tests".into());
        created.task_description = Some("...".into());
        created.teammate_name = Some("reviewer".into());
        assert_eq!(
            created.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some("Write tests".into()),
                kind: Some(NotificationKind::TaskCreated),
            })
        );
        assert_eq!(
            created.child_agent().and_then(|c| c.name),
            Some("reviewer".to_string())
        );

        // Task events from the lead itself carry no teammate.
        let mut completed = raw("TaskCompleted");
        completed.task_id = Some("t-1".into());
        completed.task_subject = Some("Write tests".into());
        assert_eq!(
            completed.to_lifecycle_event(),
            Some(LifecycleEvent::Notification {
                message: Some("Write tests".into()),
                kind: Some(NotificationKind::TaskCompleted),
            })
        );
        assert_eq!(completed.child_agent(), None);
    }

    #[test]
    fn child_alias_pairs_agent_name_with_response_id() {
        let mut done = raw("PostToolUse");
        done.tool_name = Some("Agent".into());
        done.tool_input = Some(serde_json::json!({"name": "reviewer", "prompt": "..."}));
        done.tool_response =
            Some(serde_json::json!({"status": "completed", "agentId": "ab0ba21136290a8f9"}));
        assert_eq!(
            done.child_alias(),
            Some(("reviewer".to_string(), "ab0ba21136290a8f9".to_string()))
        );

        // Unnamed spawns, other tools, and PreToolUse have no alias.
        let mut unnamed = raw("PostToolUse");
        unnamed.tool_name = Some("Agent".into());
        unnamed.tool_input = Some(serde_json::json!({"prompt": "..."}));
        unnamed.tool_response = Some(serde_json::json!({"agentId": "x"}));
        assert_eq!(unnamed.child_alias(), None);
        let mut other = raw("PostToolUse");
        other.tool_name = Some("Bash".into());
        other.tool_input = Some(serde_json::json!({"name": "n"}));
        other.tool_response = Some(serde_json::json!({"agentId": "x"}));
        assert_eq!(other.child_alias(), None);
        let mut pre = raw("PreToolUse");
        pre.tool_name = Some("Agent".into());
        pre.tool_input = Some(serde_json::json!({"name": "reviewer"}));
        assert_eq!(pre.child_alias(), None);
    }

    #[test]
    fn subagent_stop_carries_last_message_and_no_reason() {
        // Claude Code 2.1.267 emits no stop reason (see the fixture);
        // `reason` stays reserved for vendors that do.
        let mut e = raw("SubagentStop");
        e.agent_id = Some("ab0ba21136290a8f9".into());
        e.agent_type = Some("general-purpose".into());
        e.last_assistant_message = Some("subagent-hello".into());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ChildSessionEnd {
                id: Some("ab0ba21136290a8f9".into()),
                reason: None,
                last_message: Some("subagent-hello".into()),
            })
        );
    }

    #[test]
    fn child_agent_tags_events_fired_inside_a_subagent() {
        // Captured from Claude Code 2.1.267: a PreToolUse fired inside a
        // subagent keeps the parent's session_id and adds agent_id/type.
        let mut inside = raw("PreToolUse");
        inside.tool_name = Some("Bash".into());
        inside.agent_id = Some("ab0ba21136290a8f9".into());
        inside.agent_type = Some("general-purpose".into());
        assert_eq!(
            inside.child_agent(),
            Some(ChildAgentRef {
                id: Some("ab0ba21136290a8f9".into()),
                name: None,
                role: Some("general-purpose".into()),
            })
        );

        // SubagentStart/Stop carry the id but are about the child, not
        // from it: they must not be routed to the child session.
        for name in ["SubagentStart", "SubagentStop"] {
            let mut e = raw(name);
            e.agent_id = Some("ab0ba21136290a8f9".into());
            assert_eq!(e.child_agent(), None, "{name} must not be routed");
        }

        // Main-agent events have no (or an empty) agent_id.
        let mut main = raw("PreToolUse");
        main.tool_name = Some("Bash".into());
        assert_eq!(main.child_agent(), None);
        let mut empty = raw("PreToolUse");
        empty.tool_name = Some("Bash".into());
        empty.agent_id = Some(String::new());
        assert_eq!(empty.child_agent(), None);
    }

    #[test]
    fn background_activity_counts_only_on_stop() {
        let mut stop = raw("Stop");
        stop.background_tasks = Some(serde_json::json!([{"id": "t1"}, {"id": "t2"}]));
        stop.session_crons = Some(serde_json::json!([{"id": "c1"}]));
        assert_eq!(
            stop.background_activity(),
            Some(LifecycleEvent::BackgroundActivity {
                running_tasks: 2,
                scheduled_tasks: 1,
            })
        );

        // Empty arrays still report (zero) so the registry can sweep.
        let mut quiet = raw("Stop");
        quiet.background_tasks = Some(serde_json::json!([]));
        quiet.session_crons = Some(serde_json::json!([]));
        assert_eq!(
            quiet.background_activity(),
            Some(LifecycleEvent::BackgroundActivity {
                running_tasks: 0,
                scheduled_tasks: 0,
            })
        );

        // Older Claude versions without the fields: nothing to report.
        assert_eq!(raw("Stop").background_activity(), None);

        // Only Stop carries the snapshot for the session itself.
        let mut sub = raw("SubagentStop");
        sub.background_tasks = Some(serde_json::json!([]));
        assert_eq!(sub.background_activity(), None);

        // A non-array shape is tolerated and counts as zero.
        let mut odd = raw("Stop");
        odd.background_tasks = Some(serde_json::json!({"unexpected": true}));
        assert_eq!(
            odd.background_activity(),
            Some(LifecycleEvent::BackgroundActivity {
                running_tasks: 0,
                scheduled_tasks: 0,
            })
        );
    }

    /// Real hook stream from Claude Code 2.1.267 for one Agent-tool
    /// subagent run (captured with a hook that dumps stdin, paths
    /// scrubbed). Guards the routing contract against payload drift.
    #[test]
    fn captured_subagent_run_routes_child_events() {
        const CAPTURE: &str = include_str!("../tests/fixtures/claude_subagent_run_2.1.267.jsonl");
        let events: Vec<RawHookEvent> = CAPTURE
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("fixture line parses"))
            .collect();
        assert_eq!(events.len(), 10);

        // Every line is a known event.
        for e in &events {
            assert!(
                e.to_lifecycle_event().is_some(),
                "{} should translate",
                e.hook_event_name
            );
        }

        // Only the two tool hooks fired *inside* the subagent are routed
        // to the child; SubagentStart/Stop and main-agent hooks are not.
        let routed: Vec<&str> = events
            .iter()
            .filter(|e| e.child_agent().is_some())
            .map(|e| e.hook_event_name.as_str())
            .collect();
        assert_eq!(routed, ["PreToolUse", "PostToolUse"]);
        let child = events
            .iter()
            .find_map(|e| e.child_agent())
            .expect("child ref");
        assert_eq!(child.role.as_deref(), Some("general-purpose"));

        // The child lifecycle carries the same id on both ends, plus
        // how it stopped.
        let start = events.iter().find(|e| e.hook_event_name == "SubagentStart");
        let stop = events.iter().find(|e| e.hook_event_name == "SubagentStop");
        match (
            start.and_then(RawHookEvent::to_lifecycle_event),
            stop.and_then(RawHookEvent::to_lifecycle_event),
        ) {
            (
                Some(LifecycleEvent::ChildSessionStart { id: Some(a), .. }),
                Some(LifecycleEvent::ChildSessionEnd {
                    id: Some(b),
                    last_message,
                    ..
                }),
            ) => {
                assert_eq!(a, b);
                assert_eq!(Some(a), child.id);
                assert_eq!(last_message.as_deref(), Some("subagent-hello"));
            }
            other => panic!("unexpected child lifecycle: {other:?}"),
        }

        // The main agent spawned the child through the `Agent` tool.
        let spawn = events
            .iter()
            .find(|e| e.hook_event_name == "PreToolUse" && e.agent_id.is_none())
            .and_then(RawHookEvent::to_lifecycle_event);
        assert!(matches!(
            spawn,
            Some(LifecycleEvent::ToolCallStart {
                name: Tool::Agent,
                ..
            })
        ));

        // Stop carries the (empty) background snapshot; SubagentStop
        // does not report one for the session.
        let main_stop = events.iter().find(|e| e.hook_event_name == "Stop");
        assert_eq!(
            main_stop.and_then(RawHookEvent::background_activity),
            Some(LifecycleEvent::BackgroundActivity {
                running_tasks: 0,
                scheduled_tasks: 0,
            })
        );
        assert_eq!(stop.and_then(RawHookEvent::background_activity), None);
    }
}
