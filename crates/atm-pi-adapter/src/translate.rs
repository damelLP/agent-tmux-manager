//! Translation from pi `RawPiEvent` to vendor-neutral `LifecycleEvent`.
//!
//! Symmetric with `atm_claude_adapter::translate`: this is the single
//! place pi semantics map to atm-core types. Adapter-mediated
//! `NeedsInput` is synthesized here (see the [`Self::to_lifecycle_event`]
//! handling of `tool_call` payloads where `needs_user_input == true`).
//!
//! ## Mapping table (events observed in the spike)
//!
//! | pi event                | LifecycleEvent                                |
//! |-------------------------|-----------------------------------------------|
//! | `session_start`         | `SessionStart { source: reason }`             |
//! | `session_shutdown`      | `SessionEnd { reason }`                       |
//! | `agent_start`           | `WorkingStart`                                |
//! | `agent_end`             | `WorkingEnd`                                  |
//! | `input` (interactive)   | `PromptSubmit { prompt: text }`               |
//! | `tool_execution_start`  | (suppressed — `tool_call` is the canonical signal) |
//! | `tool_call`             | `ToolCallStart` *or* `NeedsInput{PermissionGate}` |
//! | `tool_execution_end`    | `ToolCallEnd { is_error }`                    |
//! | `tool_result`           | (suppressed — `tool_execution_end` is canonical) |
//! | `session_before_compact`| `ContextCompactStart`                         |
//! | `model_select`          | `ProviderModelChange { provider, model }`     |
//!
//! ### Why some events suppress
//!
//! Pi fires `tool_execution_start`/`tool_call` *both* for every tool
//! invocation, and `tool_execution_end`/`tool_result` *both* for the
//! end. We pick one of each pair to forward so downstream sees one
//! `ToolCallStart`/`ToolCallEnd` per invocation:
//!
//! - `tool_call` is forwarded (it's the permission-gate hook point and
//!   carries `input`)
//! - `tool_execution_end` is forwarded (it has the canonical
//!   `is_error` flag in pi's type union)
//! - The other two return `None` from translation, so the daemon
//!   ignores them.

use atm_core::{LifecycleEvent, NeedsInputReason, Tool};
use serde_json::Value;

use crate::event::PiEventType;
use crate::wire::RawPiEvent;

/// Extracts cumulative `(tokens, cost_usd)` from a pi `context` event's
/// `messages[]` array.
///
/// Pi's wire shape (verified by the spike trace):
/// ```jsonc
/// "messages": [
///   { "role": "user", ... },
///   { "role": "assistant",
///     "usage": {
///       "input": 1088, "output": 55, "totalTokens": 1143,
///       "cost": { "input": 0.00544, "output": 0.00165, "total": 0.00709 }
///     },
///     "provider": "openai-codex",
///     "model": "gpt-5.5",
///     ...
///   }
/// ]
/// ```
///
/// Cost values are *cumulative for the session* — pi reports the
/// running total on each assistant message. We pick the **last**
/// message that has a usage object so we always get the latest
/// cumulative numbers. Either field returns `None` if not present.
fn extract_context_usage(messages: Option<&Value>) -> (Option<u64>, Option<f64>) {
    let Some(arr) = messages.and_then(Value::as_array) else {
        return (None, None);
    };
    // Walk in reverse — the last assistant message has the freshest
    // cumulative usage.
    for msg in arr.iter().rev() {
        let Some(usage) = msg.get("usage") else {
            continue;
        };
        let tokens = usage
            .get("totalTokens")
            .and_then(Value::as_u64)
            .or_else(|| {
                // Fallback: input + output if totalTokens missing.
                let i = usage.get("input").and_then(Value::as_u64).unwrap_or(0);
                let o = usage.get("output").and_then(Value::as_u64).unwrap_or(0);
                let sum = i + o;
                if sum > 0 {
                    Some(sum)
                } else {
                    None
                }
            });
        let cost_usd = usage
            .get("cost")
            .and_then(|c| c.get("total"))
            .and_then(Value::as_f64);
        if tokens.is_some() || cost_usd.is_some() {
            return (tokens, cost_usd);
        }
    }
    (None, None)
}

impl RawPiEvent {
    /// Translates this pi raw event into a vendor-neutral
    /// `LifecycleEvent`.
    ///
    /// Returns `None` for events that should be suppressed (e.g.
    /// duplicate-of-`tool_call`) or that this adapter does not yet
    /// surface (e.g. high-frequency `message_update`,
    /// `before_provider_request`, `turn_start`).
    ///
    /// ## NeedsInput synthesis
    ///
    /// Pi has no first-class permission-prompt event; permission
    /// gating happens *inside* an extension's `tool_call` handler via
    /// `ctx.ui.select(...)`. Two adapter paths feed `NeedsInput`:
    ///
    /// 1. **Synthetic events (used by `pi-atm`).** The pi-atm
    ///    extension wraps `ctx.ui.select` and emits
    ///    `atm_needs_input_open{title}` / `atm_needs_input_resolved`
    ///    around the dialog — translated to
    ///    `NeedsInputReason::Notification{ kind: PermissionPrompt,
    ///    label }` on open and `LifecycleEvent::WorkingStart` on
    ///    resolve. This is the path actually exercised by today's
    ///    extension.
    /// 2. **Inline flag (still supported).** When a `tool_call`
    ///    payload arrives with `needs_user_input: true`, it
    ///    translates to `NeedsInputReason::PermissionGate{ tool }`.
    ///    Kept for adapters that prefer to ride the existing
    ///    `tool_call` envelope rather than emit synthetic events.
    ///
    /// Without either signal, `tool_call` is just
    /// `LifecycleEvent::ToolCallStart`.
    pub fn to_lifecycle_event(&self) -> Option<LifecycleEvent> {
        let p = &self.payload;
        Some(match self.event {
            PiEventType::SessionStart => LifecycleEvent::SessionStart {
                source: p.reason.clone(),
            },
            PiEventType::SessionShutdown => LifecycleEvent::SessionEnd {
                reason: p.reason.clone(),
            },
            PiEventType::AgentStart => LifecycleEvent::WorkingStart,
            PiEventType::AgentEnd => LifecycleEvent::WorkingEnd,

            PiEventType::Input => match p.source.as_deref() {
                Some("interactive") => LifecycleEvent::PromptSubmit {
                    prompt: p.text.clone(),
                },
                _ => return None,
            },

            // Suppress: tool_call is the canonical "tool starting" signal.
            PiEventType::ToolExecutionStart => return None,

            // Tool-shaped events without a `tool_name` are malformed
            // (Tool::from("") would inject phantom Tool::Other("")
            // records into the registry). Drop them — same guard the
            // Claude adapter applies to PreToolUse / PostToolUse.
            PiEventType::ToolCall => {
                let tool_name = p.tool_name.as_deref().filter(|s| !s.is_empty())?;
                let tool = Tool::from(tool_name);
                if p.needs_user_input.unwrap_or(false) {
                    LifecycleEvent::NeedsInput {
                        reason: NeedsInputReason::PermissionGate { tool },
                    }
                } else {
                    LifecycleEvent::ToolCallStart {
                        name: tool,
                        tool_use_id: p.tool_call_id.clone(),
                        input: p.input.clone(),
                    }
                }
            }

            PiEventType::ToolExecutionEnd => {
                let tool_name = p.tool_name.as_deref().filter(|s| !s.is_empty())?;
                LifecycleEvent::ToolCallEnd {
                    name: Tool::from(tool_name),
                    tool_use_id: p.tool_call_id.clone(),
                    is_error: p.is_error.unwrap_or(false),
                }
            }

            // Suppress: paired with tool_execution_end.
            PiEventType::ToolResult => return None,

            PiEventType::SessionBeforeCompact => {
                LifecycleEvent::ContextCompactStart { trigger: None }
            }

            PiEventType::ModelSelect => LifecycleEvent::ProviderModelChange {
                provider: p.provider.clone(),
                model: p.model.clone(),
            },

            // Synthetic events emitted by `pi-atm` when it detects
            // a `ctx.ui.select(...)` dialog opened/closed in any
            // extension (most often pi-amplike's permission gate).
            // This is the load-bearing finding from the spike encoded
            // as runtime instrumentation: pi has no permission event,
            // so the hook script wraps `ui.select` and emits these.
            PiEventType::AtmNeedsInputOpen => LifecycleEvent::NeedsInput {
                reason: atm_core::NeedsInputReason::Notification {
                    kind: atm_core::NotificationKind::PermissionPrompt,
                    // Title carries the dialog's prompt text (e.g. the
                    // bash command being gated by pi-amplike). The TUI
                    // surfaces it in `current_activity` so the user can
                    // see *what* permission is being asked, not just
                    // that *something* is.
                    label: p.title.clone(),
                },
            },
            // When the dialog closes, pi resumes work. WorkingStart
            // transitions out of NeedsInput; the next agent_end /
            // tool_call will drive further state.
            PiEventType::AtmNeedsInputResolved => LifecycleEvent::WorkingStart,

            PiEventType::Context => {
                // Pi's `context` event carries the full conversation
                // snapshot in `messages[]`. Each assistant message has
                // `usage.{cost.total, totalTokens, provider, model}`.
                // We extract just cost+tokens from the *latest*
                // message that carries usage info — pi reports
                // cumulative session-wide values there.
                let (tokens, cost_usd) = extract_context_usage(p.messages.as_ref());
                if tokens.is_none() && cost_usd.is_none() {
                    return None;
                }
                LifecycleEvent::ContextUpdate {
                    tokens,
                    current_tokens: None,
                    context_window_size: None,
                    cost_usd,
                }
            }

            // High-frequency / internal events that don't have a useful
            // user-facing translation today. Adapter author can flesh
            // these out later as we learn what the UI needs.
            PiEventType::BeforeProviderRequest
            | PiEventType::AfterProviderResponse
            | PiEventType::BeforeAgentStart
            | PiEventType::TurnStart
            | PiEventType::TurnEnd
            | PiEventType::MessageStart
            | PiEventType::MessageUpdate
            | PiEventType::MessageEnd
            | PiEventType::ToolExecutionUpdate
            | PiEventType::SessionBeforeSwitch
            | PiEventType::SessionBeforeFork
            | PiEventType::SessionCompact
            | PiEventType::SessionBeforeTree
            | PiEventType::SessionTree
            | PiEventType::ResourcesDiscover
            | PiEventType::UserBash
            | PiEventType::Other(_) => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::PiPayload;

    fn raw(event: PiEventType, payload: PiPayload) -> RawPiEvent {
        RawPiEvent {
            event,
            payload,
            session_id: None,
            pid: None,
            tmux_pane: None,
        }
    }

    #[test]
    fn atm_needs_input_open_becomes_needs_input_permission_prompt() {
        // Emitted by pi-atm when ctx.ui.select(...) is called
        // (e.g. pi-amplike's bash permission gate). Title is None
        // when the dialog has no caption.
        let e = raw(PiEventType::AtmNeedsInputOpen, PiPayload::default());
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: atm_core::NeedsInputReason::Notification {
                    kind: atm_core::NotificationKind::PermissionPrompt,
                    label: None,
                }
            })
        );
    }

    #[test]
    fn atm_needs_input_open_plumbs_title_into_label() {
        // Real shape: pi-atm forwards `title` from the
        // intercepted ctx.ui.select call so the TUI can show
        // the gated command (or whatever the dialog asked).
        let payload = PiPayload {
            title: Some("Allow `rm -rf /tmp/cache`?".to_string()),
            ..PiPayload::default()
        };
        let e = raw(PiEventType::AtmNeedsInputOpen, payload);
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: atm_core::NeedsInputReason::Notification {
                    kind: atm_core::NotificationKind::PermissionPrompt,
                    label: Some("Allow `rm -rf /tmp/cache`?".to_string()),
                }
            })
        );
    }

    #[test]
    fn context_event_extracts_latest_usage_into_context_update() {
        // Real-shape payload mirroring the spike's
        // /tmp/atm-pi-spike-pid1226148-2026-05-02 trace line.
        let messages = serde_json::json!([
            {"role": "user", "content": "Use ls to list /tmp"},
            {
                "role": "assistant",
                "content": [{"type": "toolCall"}],
                "api": "openai-codex-responses",
                "provider": "openai-codex",
                "model": "gpt-5.5",
                "usage": {
                    "input": 1088,
                    "output": 55,
                    "totalTokens": 1143,
                    "cost": {"input": 0.00544, "output": 0.00165, "total": 0.00709}
                },
                "stopReason": "toolUse"
            }
        ]);
        let e = raw(
            PiEventType::Context,
            PiPayload {
                messages: Some(messages),
                ..Default::default()
            },
        );
        match e.to_lifecycle_event() {
            Some(LifecycleEvent::ContextUpdate {
                tokens,
                current_tokens,
                context_window_size,
                cost_usd,
            }) => {
                assert_eq!(tokens, Some(1143));
                assert_eq!(current_tokens, None);
                assert_eq!(context_window_size, None);
                assert_eq!(cost_usd, Some(0.00709));
            }
            other => panic!("expected ContextUpdate, got {other:?}"),
        }
    }

    #[test]
    fn context_event_with_no_usage_returns_none() {
        let messages = serde_json::json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"} // no usage
        ]);
        let e = raw(
            PiEventType::Context,
            PiPayload {
                messages: Some(messages),
                ..Default::default()
            },
        );
        assert_eq!(e.to_lifecycle_event(), None);
    }

    #[test]
    fn atm_needs_input_resolved_becomes_working_start() {
        // After the dialog closes, pi resumes work — WorkingStart
        // transitions the session out of NeedsInput.
        let e = raw(PiEventType::AtmNeedsInputResolved, PiPayload::default());
        assert_eq!(e.to_lifecycle_event(), Some(LifecycleEvent::WorkingStart));
    }

    #[test]
    fn session_start_carries_reason_as_source() {
        let e = raw(
            PiEventType::SessionStart,
            PiPayload {
                reason: Some("startup".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );
    }

    #[test]
    fn session_shutdown_carries_reason() {
        let e = raw(
            PiEventType::SessionShutdown,
            PiPayload {
                reason: Some("quit".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("quit".into())
            })
        );
    }

    #[test]
    fn agent_start_and_end_map_to_working_boundary() {
        assert_eq!(
            raw(PiEventType::AgentStart, PiPayload::default()).to_lifecycle_event(),
            Some(LifecycleEvent::WorkingStart)
        );
        assert_eq!(
            raw(PiEventType::AgentEnd, PiPayload::default()).to_lifecycle_event(),
            Some(LifecycleEvent::WorkingEnd)
        );
    }

    #[test]
    fn interactive_input_becomes_prompt_submit() {
        let e = raw(
            PiEventType::Input,
            PiPayload {
                source: Some("interactive".into()),
                text: Some("hello pi".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::PromptSubmit {
                prompt: Some("hello pi".into())
            })
        );
    }

    #[test]
    fn non_interactive_input_is_suppressed() {
        let e = raw(
            PiEventType::Input,
            PiPayload {
                source: Some("internal".into()),
                ..Default::default()
            },
        );
        assert_eq!(e.to_lifecycle_event(), None);
    }

    #[test]
    fn tool_execution_start_is_suppressed_in_favor_of_tool_call() {
        // The adapter forwards `tool_call`, not `tool_execution_start`,
        // because tool_call is the permission-gate hook point and
        // carries `input`.
        let e = raw(
            PiEventType::ToolExecutionStart,
            PiPayload {
                tool_name: Some("ls".into()),
                tool_call_id: Some("call_xyz".into()),
                ..Default::default()
            },
        );
        assert_eq!(e.to_lifecycle_event(), None);
    }

    #[test]
    fn tool_call_becomes_tool_call_start_with_correlation_id() {
        let e = raw(
            PiEventType::ToolCall,
            PiPayload {
                tool_name: Some("ls".into()),
                tool_call_id: Some("call_xyz".into()),
                input: Some(serde_json::json!({"path":"/tmp"})),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallStart {
                // pi uses lowercase "ls"; lands in Tool::Other since
                // ATM's Tool enum only canonicalizes Claude PascalCase
                // and well-known names.
                name: Tool::Other("ls".into()),
                tool_use_id: Some("call_xyz".into()),
                input: Some(serde_json::json!({"path":"/tmp"})),
            })
        );
    }

    #[test]
    fn tool_call_with_known_tool_uses_canonical_variant() {
        // If pi happens to emit a tool name we recognize (e.g. "Bash"
        // when wrapping a Claude bridge), we land in the canonical
        // variant — same Tool::Bash as the Claude side.
        let e = raw(
            PiEventType::ToolCall,
            PiPayload {
                tool_name: Some("Bash".into()),
                tool_call_id: Some("call_42".into()),
                ..Default::default()
            },
        );
        match e.to_lifecycle_event() {
            Some(LifecycleEvent::ToolCallStart { name, .. }) => {
                assert_eq!(name, Tool::Bash);
            }
            other => panic!("expected ToolCallStart, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_with_needs_user_input_becomes_permission_gate() {
        // Pi has no permission event. The TS extension synthesizes
        // this by setting needs_user_input=true when it reaches
        // ctx.ui.select(...). This is THE load-bearing finding from
        // the spike.
        let e = raw(
            PiEventType::ToolCall,
            PiPayload {
                tool_name: Some("bash".into()),
                tool_call_id: Some("call_dangerous".into()),
                needs_user_input: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate {
                    tool: Tool::Other("bash".into())
                }
            })
        );
    }

    #[test]
    fn tool_shaped_event_without_tool_name_returns_none() {
        // Symmetric with the Claude adapter's guard: pi `tool_call`
        // and `tool_execution_end` payloads without a `tool_name` are
        // malformed. Translating them used to produce
        // `Tool::Other("")` and inject phantom records into the
        // registry. Verify the guard drops them across both `None`
        // and empty-string forms.
        for ev in [PiEventType::ToolCall, PiEventType::ToolExecutionEnd] {
            assert_eq!(
                raw(ev.clone(), PiPayload::default()).to_lifecycle_event(),
                None,
                "{ev:?} with tool_name=None should drop"
            );
            assert_eq!(
                raw(
                    ev.clone(),
                    PiPayload {
                        tool_name: Some(String::new()),
                        ..Default::default()
                    }
                )
                .to_lifecycle_event(),
                None,
                "{ev:?} with empty tool_name should drop"
            );
        }
    }

    #[test]
    fn tool_execution_end_carries_error_flag_and_correlation() {
        let ok = raw(
            PiEventType::ToolExecutionEnd,
            PiPayload {
                tool_name: Some("ls".into()),
                tool_call_id: Some("call_xyz".into()),
                is_error: Some(false),
                ..Default::default()
            },
        );
        let err = raw(
            PiEventType::ToolExecutionEnd,
            PiPayload {
                tool_name: Some("ls".into()),
                tool_call_id: Some("call_xyz".into()),
                is_error: Some(true),
                ..Default::default()
            },
        );

        assert_eq!(
            ok.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Other("ls".into()),
                tool_use_id: Some("call_xyz".into()),
                is_error: false,
            })
        );
        assert_eq!(
            err.to_lifecycle_event(),
            Some(LifecycleEvent::ToolCallEnd {
                name: Tool::Other("ls".into()),
                tool_use_id: Some("call_xyz".into()),
                is_error: true,
            })
        );
    }

    #[test]
    fn tool_result_is_suppressed_in_favor_of_tool_execution_end() {
        let e = raw(
            PiEventType::ToolResult,
            PiPayload {
                tool_name: Some("ls".into()),
                ..Default::default()
            },
        );
        assert_eq!(e.to_lifecycle_event(), None);
    }

    #[test]
    fn model_select_becomes_provider_model_change() {
        let e = raw(
            PiEventType::ModelSelect,
            PiPayload {
                provider: Some("openai-codex".into()),
                model: Some("gpt-5.5".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ProviderModelChange {
                provider: Some("openai-codex".into()),
                model: Some("gpt-5.5".into()),
            })
        );
    }

    #[test]
    fn session_before_compact_becomes_context_compact_start() {
        assert_eq!(
            raw(PiEventType::SessionBeforeCompact, PiPayload::default()).to_lifecycle_event(),
            Some(LifecycleEvent::ContextCompactStart { trigger: None })
        );
    }

    #[test]
    fn unknown_event_returns_none() {
        let e = raw(
            PiEventType::Other("hypothetical_future".into()),
            PiPayload::default(),
        );
        assert_eq!(e.to_lifecycle_event(), None);
    }

    // ========================================================================
    // FEATURE PARITY: Claude vs pi produce same session-state outcome
    //
    // These tests are the literal proof that the LifecycleEvent
    // abstraction lets two different vendors drive the same downstream
    // session state. Each test pairs an equivalent operation across
    // the two adapters and asserts the LifecycleEvents are equal.
    // ========================================================================

    #[test]
    fn parity_session_lifecycle_matches_claude_shape() {
        // Pi: session_start with reason="startup"
        let pi_start = raw(
            PiEventType::SessionStart,
            PiPayload {
                reason: Some("startup".into()),
                ..Default::default()
            },
        );
        // The Claude side shape (mirrored — we just construct it
        // here to assert structural equality, since the Claude
        // translation lives in a different crate).
        let expected = LifecycleEvent::SessionStart {
            source: Some("startup".into()),
        };
        assert_eq!(pi_start.to_lifecycle_event(), Some(expected));
    }

    #[test]
    fn parity_tool_invocation_produces_same_shape() {
        // Pi tool_call with toolName/toolCallId
        let pi_tool = raw(
            PiEventType::ToolCall,
            PiPayload {
                tool_name: Some("Bash".into()),
                tool_call_id: Some("toolu_abc".into()),
                input: Some(serde_json::json!({"command":"ls"})),
                ..Default::default()
            },
        );
        // Equivalent Claude PreToolUse shape:
        let expected = LifecycleEvent::ToolCallStart {
            name: Tool::Bash,
            tool_use_id: Some("toolu_abc".into()),
            input: Some(serde_json::json!({"command":"ls"})),
        };
        assert_eq!(pi_tool.to_lifecycle_event(), Some(expected));
    }
}
