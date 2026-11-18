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
//! | `agent_settled`         | `WorkingEnd`                                  |
//! | `agent_end` (legacy forwarders only) | `WorkingEnd`                        |
//! | `input` (interactive)   | `PromptSubmit { prompt: text }`               |
//! | `atm_idle`              | `Idle` (real id and nonzero pid required)     |
//! | `atm_needs_input_open`  | `NeedsInput` with the select title           |
//! | `atm_needs_input_resolved` | `Idle` iff payload.idle=true, else `WorkingStart` |
//! | `tool_execution_start`  | (suppressed — `tool_call` is the canonical signal) |
//! | `tool_call`             | `ToolCallStart` *or* `NeedsInput{PermissionGate}` |
//! | `tool_execution_end`    | `ToolCallEnd { is_error }`                    |
//! | `tool_result`           | (suppressed — `tool_execution_end` is canonical) |
//! | `session_before_compact`| `ContextCompactStart`                         |
//! | `session_compact` / `session_compact_failed` (manual) | `Idle`             |
//! | `model_select`          | `ProviderModelChange { provider, model }`     |
//! | `context`, `turn_end`   | `ContextUpdate` from pi-atm's usage snapshot  |
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

/// Extracts `(tokens, cost_usd)` from a pi `context` event's
/// `messages[]` array.
///
/// Pi's wire shape. Captured: pi, version not recorded, 2026-05-02
/// spike trace (see the crate docs). pi 0.73.1 sends the same shape
/// (`tests/fixtures/pi_tool_run_0.73.1.jsonl`).
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
/// Usage and cost are *per request*, not session totals: pi prices each
/// reply on its own. Captured: pi 0.73.1, 2026-10-24, the fixture above,
/// whose two replies cost 0.0033 and 0.003675. We pick the **last**
/// message that has a usage object, so the result covers the latest
/// request only. Used only for frames from pi-atm before its usage
/// snapshot (see the crate docs' known gaps). Either field returns
/// `None` if not present.
fn extract_context_usage(messages: Option<&Value>) -> (Option<u64>, Option<f64>) {
    let Some(arr) = messages.and_then(Value::as_array) else {
        return (None, None);
    };
    // Walk in reverse — the last assistant message has the freshest
    // usage.
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
                // Oversized legacy counts must not panic or wrap.
                let sum = i.saturating_add(o);
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
    /// Cumulative accounting only; the legacy context-message fallback is one
    /// request's cost, not a session total. Leave cockpit compatibility intact.
    /// Captured: pi 0.73.1, 2026-10-24, pi_session_cost fixture/socket tap.
    #[must_use]
    pub fn accounting_usage(&self) -> Option<LifecycleEvent> {
        self.usage?;
        match self.to_lifecycle_event()? {
            event @ LifecycleEvent::ContextUpdate { .. } => Some(event),
            _ => None,
        }
    }

    /// Complete translation: the original event, then any derived metadata.
    /// Only the first event is genuine activity; later events update registry
    /// metadata without entering message-delivery evidence. Never resurrect a
    /// shutdown or turn a suppressed primary event into a metadata-only row.
    #[must_use]
    pub fn to_lifecycle_events(&self) -> Vec<LifecycleEvent> {
        let Some(event) = self.to_lifecycle_event() else {
            return Vec::new();
        };
        let model = match event {
            LifecycleEvent::SessionEnd { .. } | LifecycleEvent::ProviderModelChange { .. } => None,
            _ => self
                .current_model
                .as_ref()
                .map(|model| LifecycleEvent::ProviderModelChange {
                    provider: model.provider.clone(),
                    model: Some(model.id.clone()),
                }),
        };
        std::iter::once(event).chain(model).collect()
    }

    /// Translates the primary event only. Use [`Self::to_lifecycle_events`]
    /// for the complete translation, including the envelope's model snapshot.
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
    ///    label }` on open, then Idle for measured idle=true or WorkingStart
    ///    otherwise on resolve. This is the path actually exercised by today's
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
            PiEventType::AgentEnd | PiEventType::AgentSettled => LifecycleEvent::WorkingEnd,

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

            // Manual compaction has no agent_settled. Automatic compaction
            // may retry or continue, so its outcomes must await settlement.
            PiEventType::SessionCompact | PiEventType::SessionCompactFailed => {
                if p.reason.as_deref() != Some("manual") {
                    return None;
                }
                LifecycleEvent::Idle
            }

            PiEventType::AtmIdle => {
                // Registration is real identity only; never take legacy PID
                // attribution's pending-id fallback for this new event.
                let id = atm_core::SessionId::new(self.session_id.as_ref()?.clone());
                if id.as_str().is_empty() || id.is_pending() || self.pid.unwrap_or(0) == 0 {
                    return None;
                }
                LifecycleEvent::Idle
            }

            PiEventType::ModelSelect => LifecycleEvent::ProviderModelChange {
                provider: p.model.as_ref().and_then(|m| m.provider.clone()),
                model: p.model.as_ref().map(|m| m.id.clone()),
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
            // Only affirmative closure-time measurement clears work. Missing
            // measurements (older forwarders or input veto) retain the fallback.
            PiEventType::AtmNeedsInputResolved => {
                if p.idle == Some(true) {
                    LifecycleEvent::Idle
                } else {
                    LifecycleEvent::WorkingStart
                }
            }

            PiEventType::Context | PiEventType::TurnEnd => {
                // pi-atm's usage snapshot carries the session's totals.
                // pi-atm before the snapshot forwards only `context`, whose
                // latest reply gives one request's figures.
                let (tokens, current_tokens, cost_usd) = match (&self.usage, &self.event) {
                    // Session tokens go only with the context size: alone,
                    // atm-core would show them as the active context.
                    (Some(u), _) => (u.context_tokens.and(u.tokens), u.context_tokens, u.cost_usd),
                    (None, PiEventType::Context) => {
                        let (tokens, cost_usd) = extract_context_usage(p.messages.as_ref());
                        (tokens, None, cost_usd)
                    }
                    (None, _) => return None,
                };
                // pi's events carry no window; pi-atm's envelope snapshot
                // of the current model does. It goes through even before
                // the first reply has usage.
                let context_window_size =
                    self.current_model.as_ref().and_then(|m| m.context_window);
                if tokens.is_none()
                    && current_tokens.is_none()
                    && cost_usd.is_none()
                    && context_window_size.is_none()
                {
                    return None;
                }
                LifecycleEvent::ContextUpdate {
                    tokens,
                    current_tokens,
                    context_window_size,
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
            | PiEventType::MessageStart
            | PiEventType::MessageUpdate
            | PiEventType::MessageEnd
            | PiEventType::ToolExecutionUpdate
            | PiEventType::SessionBeforeSwitch
            | PiEventType::SessionBeforeFork
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
    #[test]
    fn pi_1_5_2_idle_registration() {
        // Synthetic: proposed registration framing; real changed-forwarder
        // capture is required before claiming vendor capture provenance.
        let frame = serde_json::json!({
            "event": "atm_idle", "payload": {}, "session_id": "real-session", "pid": 123,
            "model": {"id":"probe", "provider":"synthetic", "contextWindow":8192},
            "usage": {"tokens":42, "cost":1.0, "contextTokens":10}
        });
        let raw: crate::RawPiEvent = serde_json::from_value(frame.clone()).unwrap();
        assert_eq!(
            raw.to_lifecycle_events(),
            vec![
                atm_core::LifecycleEvent::Idle,
                atm_core::LifecycleEvent::ProviderModelChange {
                    provider: Some("synthetic".into()),
                    model: Some("probe".into()),
                },
            ]
        );
        assert!(raw.accounting_usage().is_none());
        let mut no_model = frame.clone();
        no_model.as_object_mut().unwrap().remove("model");
        assert_eq!(
            serde_json::from_value::<crate::RawPiEvent>(no_model)
                .unwrap()
                .to_lifecycle_events(),
            vec![atm_core::LifecycleEvent::Idle]
        );
        for (field, bad) in [
            ("session_id", serde_json::Value::Null),
            ("session_id", serde_json::json!("")),
            ("session_id", serde_json::json!("pending-123")),
            ("pid", serde_json::Value::Null),
            ("pid", serde_json::json!(0)),
        ] {
            let mut malformed = frame.clone();
            malformed[field] = bad;
            assert!(
                serde_json::from_value::<crate::RawPiEvent>(malformed)
                    .unwrap()
                    .to_lifecycle_events()
                    .is_empty(),
                "{field}"
            );
        }
    }

    #[test]
    fn pi_1_5_2_idle_registration_capture() {
        // Captured: pi 1.5.2, 2026-11-16, sealed CLI/private tmux/atmd proxy
        // plus held-input SDK probe; details in crate docs. IDs/PIDs redacted.
        let mut snapshots = 0;
        for line in include_str!("../tests/fixtures/pi_reconnect_1.5.2.jsonl").lines() {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let raw: crate::RawPiEvent = serde_json::from_value(frame["data"].clone()).unwrap();
            if raw.event != PiEventType::AtmIdle {
                continue;
            }
            assert!(matches!(
                raw.to_lifecycle_events().as_slice(),
                [
                    LifecycleEvent::Idle,
                    LifecycleEvent::ProviderModelChange { .. }
                ]
            ));
            assert!(raw.usage.is_none());
            assert!(raw.accounting_usage().is_none());
            snapshots += 1;
        }
        assert_eq!(snapshots, 6);
    }

    #[test]
    fn legacy_per_request_usage_is_not_a_cumulative_accounting_sample() {
        // Synthetic: old extension frame, deliberately without the usage snapshot.
        let raw: crate::RawPiEvent = serde_json::from_value(serde_json::json!({"event":"context", "payload":{"messages":[{"role":"assistant","usage":{"totalTokens":10,"cost":{"total":0.5}}}]}})).unwrap();
        assert!(raw.to_lifecycle_event().is_some());
        assert!(raw.accounting_usage().is_none());
    }

    use super::*;
    use crate::wire::{PiModel, PiPayload};

    fn raw(event: PiEventType, payload: PiPayload) -> RawPiEvent {
        RawPiEvent {
            event,
            payload,
            session_id: None,
            pid: None,
            tmux_pane: None,
            current_model: None,
            usage: None,
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
        // Synthetic: no permission-gate run has been captured.
        // pi-atm forwards `title` from the
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
        // Captured: pi, version not recorded, 2026-05-02 spike trace
        // (see the crate docs).
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
        // Synthetic: hand-made messages. The real no-usage case is the
        // fixture's first `context` (see captured_tool_run_translates).
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
    fn legacy_context_usage_overflow_saturates_tokens() {
        // Synthetic: unbounded legacy JSON counters must not panic or wrap.
        let e = frame(serde_json::json!({
            "event": "context",
            "payload": {"messages": [
                {"role": "assistant", "usage": {"input": u64::MAX, "output": 1}}
            ]},
        }));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextUpdate {
                tokens: Some(u64::MAX),
                current_tokens: None,
                context_window_size: None,
                cost_usd: None,
            })
        );
        assert_eq!(e.accounting_usage(), None);
    }

    #[test]
    fn legacy_context_usage_fallback_boundaries() {
        // Synthetic: saturating addition preserves ordinary, missing and zero counts.
        for (mut usage, tokens) in [
            (
                serde_json::json!({"input": u64::MAX, "output": u64::MAX}),
                Some(u64::MAX),
            ),
            (
                serde_json::json!({"input": u64::MAX, "output": 0}),
                Some(u64::MAX),
            ),
            (
                serde_json::json!({"input": 0, "output": u64::MAX}),
                Some(u64::MAX),
            ),
            (serde_json::json!({"input": 1088, "output": 55}), Some(1143)),
            (serde_json::json!({"input": 0, "output": 0}), None),
            (serde_json::json!({"input": 1088}), Some(1088)),
            (serde_json::json!({"output": 55}), Some(55)),
            (serde_json::json!({}), None),
        ] {
            usage["cost"] = serde_json::json!({"total": 0.25});
            let messages = serde_json::json!([{"role": "assistant", "usage": usage}]);
            assert_eq!(
                extract_context_usage(Some(&messages)),
                (tokens, Some(0.25)),
                "{messages}"
            );
        }
    }

    #[test]
    fn legacy_context_total_tokens_precedes_overflowing_parts() {
        // Synthetic: an explicit total, including zero, must bypass the fallback.
        for total in [7_u64, 0] {
            let messages = serde_json::json!([
                {"role": "assistant", "usage": {
                    "totalTokens": total, "input": u64::MAX, "output": 1
                }}
            ]);
            assert_eq!(extract_context_usage(Some(&messages)), (Some(total), None));
        }
    }

    #[test]
    fn legacy_context_usage_keeps_latest_cost() {
        // Synthetic: an oversized latest sample must not fall back to older usage.
        let messages = serde_json::json!([
            {"role": "assistant", "usage": {"totalTokens": 3, "cost": {"total": 0.1}}},
            {"role": "assistant", "usage": {
                "input": u64::MAX, "output": 1, "cost": {"total": 0.25}
            }}
        ]);
        assert_eq!(
            extract_context_usage(Some(&messages)),
            (Some(u64::MAX), Some(0.25))
        );
    }

    /// A pi-atm frame's `data`, parsed as atmd parses it.
    fn frame(data: Value) -> RawPiEvent {
        serde_json::from_value(data).expect("pi_event data")
    }

    #[test]
    fn turn_end_usage_snapshot_becomes_context_update() {
        // Synthetic: pi-atm's snapshot, with the totals pi 0.73.1 reported
        // after a tool call and its reply (0.0033 + 0.003675). The real
        // frames are in captured_session_cost_translates.
        let e = frame(serde_json::json!({
            "event": "turn_end",
            "payload": {},
            "model": model_snapshot("fake-b", 32_000),
            "usage": {"cost": 0.006975, "tokens": 2225, "contextTokens": 1205},
        }));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextUpdate {
                tokens: Some(2225),
                current_tokens: Some(1205),
                context_window_size: Some(32_000),
                cost_usd: Some(0.006975),
            })
        );
    }

    #[test]
    fn context_takes_the_usage_snapshot_over_the_latest_reply() {
        // Synthetic. The latest reply cost 0.0033 on its own; the snapshot
        // carries the session's total.
        let e = frame(serde_json::json!({
            "event": "context",
            "payload": {"messages": [
                {"role": "assistant", "usage": {"totalTokens": 1020, "cost": {"total": 0.0033}}}
            ]},
            "usage": {"cost": 0.006975, "tokens": 2225, "contextTokens": 1023},
        }));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextUpdate {
                tokens: Some(2225),
                current_tokens: Some(1023),
                context_window_size: None,
                cost_usd: Some(0.006975),
            })
        );
    }

    #[test]
    fn unknown_context_size_sends_the_cost_only() {
        // Synthetic: after a compaction, pi doesn't know the context size
        // until the next reply (`getContextUsage().tokens` is null). Sent
        // alone, the session's tokens would show as the active context
        // (atm-core's `current_tokens.or(tokens)`).
        let e = frame(serde_json::json!({
            "event": "turn_end",
            "payload": {},
            "model": model_snapshot("fake-b", 32_000),
            "usage": {"cost": 0.01065, "tokens": 3430, "contextTokens": null},
        }));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextUpdate {
                tokens: None,
                current_tokens: None,
                context_window_size: Some(32_000),
                cost_usd: Some(0.01065),
            })
        );
    }

    #[test]
    fn empty_usage_snapshot_keeps_the_latest_reply_out() {
        // Synthetic: pi-atm sends `usage: {}` when it can't read pi's
        // session. The latest reply's cost (one request's) must not
        // replace the session's total.
        let e = frame(serde_json::json!({
            "event": "context",
            "payload": {"messages": [
                {"role": "assistant", "usage": {"totalTokens": 1205, "cost": {"total": 0.003675}}}
            ]},
            "model": model_snapshot("fake-b", 32_000),
            "usage": {},
        }));
        assert_eq!(
            e.to_lifecycle_event(),
            Some(LifecycleEvent::ContextUpdate {
                tokens: None,
                current_tokens: None,
                context_window_size: Some(32_000),
                cost_usd: None,
            })
        );
    }

    #[test]
    fn turn_end_without_a_usage_snapshot_returns_none() {
        // Synthetic. pi-atm forwards turn_end only with a snapshot; the
        // snapshot can still be missing if reading pi's session failed.
        for usage in [Value::Null, serde_json::json!("junk")] {
            let e = frame(serde_json::json!({
                "event": "turn_end",
                "payload": {},
                "model": model_snapshot("fake-b", 32_000),
                "usage": usage,
            }));
            assert_eq!(e.to_lifecycle_event(), None, "usage {usage}");
        }
    }

    #[test]
    fn pi_1_5_2_dialog_idle_resolution() {
        // Synthetic: proposed dialog payload and compatibility/attribution twins.
        let open = frame(serde_json::json!({
            "event":"atm_needs_input_open", "payload":{"title":"Neutral selection"}
        }));
        assert_eq!(
            open.to_lifecycle_events(),
            vec![LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: atm_core::NotificationKind::PermissionPrompt,
                    label: Some("Neutral selection".into()),
                },
            }]
        );
        for (payload, expected) in [
            (serde_json::json!({"idle":true}), LifecycleEvent::Idle),
            (
                serde_json::json!({"idle":false}),
                LifecycleEvent::WorkingStart,
            ),
            (serde_json::json!({}), LifecycleEvent::WorkingStart),
            (
                serde_json::json!({"idle":null}),
                LifecycleEvent::WorkingStart,
            ),
        ] {
            for model in [
                serde_json::Value::Null,
                serde_json::json!({"id":"probe","provider":"sealed"}),
            ] {
                let raw = frame(serde_json::json!({
                    "event":"atm_needs_input_resolved", "payload":payload, "model":model,
                    "usage":{"tokens":42,"contextTokens":10,"cost":1.0}
                }));
                let mut batch = vec![expected.clone()];
                if !model.is_null() {
                    batch.push(LifecycleEvent::ProviderModelChange {
                        provider: Some("sealed".into()),
                        model: Some("probe".into()),
                    });
                }
                assert_eq!(raw.to_lifecycle_events(), batch, "payload {payload}");
                assert!(raw.accounting_usage().is_none());
            }
        }
        for (event, payload, expected) in [
            (
                "input",
                serde_json::json!({"source":"interactive","text":"neutral","idle":true}),
                vec![LifecycleEvent::PromptSubmit {
                    prompt: Some("neutral".into()),
                }],
            ),
            (
                "agent_end",
                serde_json::json!({}),
                vec![LifecycleEvent::WorkingEnd],
            ),
            ("future_pi_event", serde_json::json!({"idle":true}), vec![]),
        ] {
            assert_eq!(
                frame(serde_json::json!({"event":event,"payload":payload})).to_lifecycle_events(),
                expected
            );
        }
    }

    #[test]
    fn pi_1_5_2_dialog_idle_resolution_capture() {
        // Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY/local select
        // command, changed forwarder and private real atmd recording proxy.
        let mut closed = 0;
        for line in include_str!("../tests/fixtures/pi_dialog_1.5.2.jsonl").lines() {
            let frame: serde_json::Value = serde_json::from_str(line).unwrap();
            let raw: RawPiEvent = serde_json::from_value(frame["data"].clone()).unwrap();
            if raw.event == PiEventType::AtmNeedsInputResolved {
                assert_eq!(raw.payload.idle, Some(true));
                assert_eq!(raw.to_lifecycle_events(), vec![LifecycleEvent::Idle]);
                assert!(raw.accounting_usage().is_none());
                assert!(raw.usage.is_none());
                closed += 1;
            }
        }
        assert_eq!(closed, 2, "Enter completion and Esc cancellation");
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
    fn pi_1_5_2_terminal_mapping() {
        // Captured: pi 1.5.2, 2026-11-16, sealed interactive PTY/local
        // static provider, extension callbacks and private atmd socket tap.
        let mut settled = 0;
        let mut manual = 0;
        let mut automatic = 0;
        let mut actual = Vec::new();
        let mut wanted = Vec::new();
        for line in include_str!("../tests/fixtures/pi_turn_events_1.5.2.jsonl").lines() {
            let capture: Value = serde_json::from_str(line).unwrap();
            let name = capture["event"].as_str().unwrap();
            let expected = match name {
                "agent_settled" => {
                    settled += 1;
                    Some(LifecycleEvent::WorkingEnd)
                }
                "session_compact" | "session_compact_failed" => {
                    if capture["payload"]["reason"] == "manual" {
                        manual += 1;
                        Some(LifecycleEvent::Idle)
                    } else {
                        automatic += 1;
                        None
                    }
                }
                _ => continue,
            };
            let event: RawPiEvent = serde_json::from_value(capture).unwrap();
            actual.push((event.event.as_str().to_owned(), event.to_lifecycle_event()));
            wanted.push((event.event.as_str().to_owned(), expected));
        }
        assert!(settled > 0 && manual > 0 && automatic > 0);
        assert_eq!(actual, wanted);
        // Synthetic: absent/unknown reasons must not end an automatic run.
        for name in ["session_compact", "session_compact_failed"] {
            for reason in [Value::Null, Value::String("future".into())] {
                let event: RawPiEvent = serde_json::from_value(serde_json::json!({
                    "event": name, "payload": {"reason": reason}
                }))
                .unwrap();
                assert_eq!(event.to_lifecycle_event(), None);
            }
        }
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
        // Synthetic: `internal` isn't a pi 0.73.1 source (those are
        // interactive, rpc and extension).
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
        // Synthetic: pi 0.73.1's own tools are lowercase (`bash`).
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
        // Synthetic: no pi-atm version has sent needs_user_input; it
        // signals the gate with atm_needs_input_open instead. This
        // covers the inline-flag path `to_lifecycle_event` still
        // supports.
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
                model: Some(PiModel {
                    id: "gpt-5.5".into(),
                    provider: Some("openai-codex".into()),
                    context_window: None,
                }),
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

    /// A model as pi-atm picks it, for the envelope and for model_select,
    /// with the captures' provider.
    fn model_snapshot(id: &str, context_window: u32) -> Value {
        serde_json::json!({"id": id, "provider": "atmfake", "contextWindow": context_window})
    }

    /// True if `key` names a field anywhere in `value`.
    fn has_key(value: &Value, key: &str) -> bool {
        match value {
            Value::Object(map) => map.contains_key(key) || map.values().any(|v| has_key(v, key)),
            Value::Array(items) => items.iter().any(|v| has_key(v, key)),
            _ => false,
        }
    }

    #[test]
    fn captured_model_select_carries_only_picked_model_fields() {
        // Captured: pi 0.62.0, 2026-10-24, set up as for
        // captured_tool_run_translates, with Ctrl+P from fake-a to fake-b
        // and no prompt. The provider sets a probe header and `authHeader`
        // with a fake key, which pi 0.62.0 resolves onto the Model object:
        // pi-atm used to forward model_select's `model` and `previousModel`
        // whole, `Authorization: Bearer <apiKey>` included. It now picks
        // the fields atmd reads. This checks the capture; no test runs
        // pi-atm.ts itself.
        let frames: Vec<Value> = include_str!("../tests/fixtures/pi_model_select_0.62.0.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("fixture line"))
            .collect();
        let [_, select, _] = frames.as_slice() else {
            panic!("expected 3 frames, got {}", frames.len());
        };

        for frame in &frames {
            assert!(!has_key(frame, "headers"), "headers in {frame}");
        }

        let payload = &select["data"]["payload"];
        assert_eq!(payload["model"], model_snapshot("fake-b", 32_000));
        assert_eq!(payload["previousModel"], model_snapshot("fake-a", 128_000));
        assert_eq!(payload["source"], "cycle");
        let raw: RawPiEvent =
            serde_json::from_value(select["data"].clone()).expect("pi_event data");
        assert_eq!(
            raw.to_lifecycle_event(),
            Some(LifecycleEvent::ProviderModelChange {
                provider: Some("atmfake".into()),
                model: Some("fake-b".into()),
            })
        );
    }

    #[test]
    fn captured_tool_run_translates() {
        // Captured: pi 0.73.1, 2026-10-24, pi's TUI in a private tmux
        // server with pi-atm installed by `atm setup`, each frame tapped
        // byte for byte by `socat -r` on its way to an isolated atmd. A
        // local fake OpenAI-compatible provider answered (no model call);
        // Ctrl+P cycled fake-a to fake-b.
        let frames: Vec<Value> = include_str!("../tests/fixtures/pi_tool_run_0.73.1.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("fixture line"))
            .collect();

        // pi 0.73.1 nests the provider in `model`; there is no top-level
        // `provider`.
        let model_payload = &frames[1]["data"]["payload"];
        assert_eq!(model_payload["model"]["provider"], "atmfake");
        assert_eq!(model_payload.get("provider"), None);

        // The provider sets a header and `authHeader`, but pi 0.73.1 keeps
        // them off the Model object (it resolves them per request).
        for frame in &frames {
            assert!(!has_key(frame, "headers"), "headers in {frame}");
        }

        // pi-atm's envelope carries a snapshot of pi's current model with
        // only the fields it picks. Ctrl+P switched fake-a to fake-b, so
        // the frames from model_select on carry fake-b.
        assert_eq!(
            frames[0]["data"]["model"],
            model_snapshot("fake-a", 128_000)
        );
        for frame in &frames[1..] {
            assert_eq!(frame["data"]["model"], model_snapshot("fake-b", 32_000));
        }

        let raws: Vec<RawPiEvent> = frames
            .iter()
            .map(|frame| serde_json::from_value(frame["data"].clone()).expect("pi_event data"))
            .collect();
        // pi-atm's envelope: pid, tmux_pane and pi's session id on every
        // frame. atmd keys the session by pid and names it by session_id.
        for raw in &raws {
            assert_eq!(raw.pid, Some(1_454_791));
            assert_eq!(raw.tmux_pane.as_deref(), Some("%0"));
            assert_eq!(
                raw.session_id.as_deref(),
                Some("01a16f2a-1686-763b-9a72-a25b2d29742c")
            );
        }

        let events: Vec<Option<LifecycleEvent>> =
            raws.iter().map(RawPiEvent::to_lifecycle_event).collect();
        let [start, model, input, working, first_context, tool_start, tool_end, context, working_end, end] =
            events.as_slice()
        else {
            panic!("expected 10 frames, got {}", events.len());
        };

        assert_eq!(
            start,
            &Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );
        assert_eq!(
            model,
            &Some(LifecycleEvent::ProviderModelChange {
                provider: Some("atmfake".into()),
                model: Some("fake-b".into()),
            })
        );
        assert_eq!(
            input,
            &Some(LifecycleEvent::PromptSubmit {
                prompt: Some("Read note.txt and reply.".into())
            })
        );
        assert_eq!(working, &Some(LifecycleEvent::WorkingStart));
        // The first `context` fires before any reply, so it has no usage,
        // but the window still goes through.
        assert_eq!(
            first_context,
            &Some(LifecycleEvent::ContextUpdate {
                tokens: None,
                current_tokens: None,
                context_window_size: Some(32_000),
                cost_usd: None,
            })
        );
        assert_eq!(
            tool_start,
            &Some(LifecycleEvent::ToolCallStart {
                name: Tool::from("read"),
                tool_use_id: Some("call_f3probe".into()),
                input: Some(serde_json::json!({"path": "note.txt"})),
            })
        );
        assert_eq!(
            tool_end,
            &Some(LifecycleEvent::ToolCallEnd {
                name: Tool::from("read"),
                tool_use_id: Some("call_f3probe".into()),
                is_error: false,
            })
        );
        // The window comes from the envelope's snapshot: fake-b's 32000,
        // not the unknown model's 200K default.
        assert_eq!(
            context,
            &Some(LifecycleEvent::ContextUpdate {
                tokens: Some(1020),
                current_tokens: None,
                context_window_size: Some(32_000),
                cost_usd: Some(0.0033),
            })
        );
        assert_eq!(working_end, &Some(LifecycleEvent::WorkingEnd));
        assert_eq!(
            end,
            &Some(LifecycleEvent::SessionEnd {
                reason: Some("quit".into())
            })
        );
    }

    #[test]
    fn empty_context_bodies_keep_captured_usage_and_identity() {
        // Synthetic: remove only context/turn_end bodies from the real capture.
        // Captured: pi 0.73.1, 2026-10-24, private TUI socket tap and local
        // fake provider; see the crate docs. The envelope is unchanged.
        let mut samples = Vec::new();
        for line in include_str!("../tests/fixtures/pi_session_cost_0.73.1.jsonl").lines() {
            let envelope: Value = serde_json::from_str(line).unwrap();
            let mut data = envelope["data"].clone();
            if !matches!(data["event"].as_str(), Some("context" | "turn_end")) {
                continue;
            }
            data["payload"] = serde_json::json!({});
            let raw = frame(data);
            assert_eq!(
                raw.session_id.as_deref(),
                Some("01a16f5b-6c34-7238-80bd-c31c15009fe9")
            );
            assert_eq!(raw.pid, Some(1_613_778));
            assert_eq!(raw.tmux_pane.as_deref(), Some("%0"));
            let events = raw.to_lifecycle_events();
            let [LifecycleEvent::ContextUpdate {
                tokens,
                current_tokens,
                context_window_size,
                cost_usd,
            }, model] = events.as_slice()
            else {
                panic!("expected usage and model, got {events:?}");
            };
            assert_eq!(
                model,
                &LifecycleEvent::ProviderModelChange {
                    provider: Some("atmfake".into()),
                    model: Some("fake-b".into()),
                }
            );
            samples.push((*tokens, *current_tokens, *context_window_size, *cost_usd));
        }
        assert_eq!(
            samples,
            [
                (Some(0), Some(6), Some(32_000), Some(0.0)),
                (Some(1020), Some(1025), Some(32_000), Some(0.0033)),
                (Some(1020), Some(1025), Some(32_000), Some(0.0033)),
                (Some(2225), Some(1205), Some(32_000), Some(0.006975)),
                (Some(2225), Some(1207), Some(32_000), Some(0.006975)),
                (Some(3430), Some(1205), Some(32_000), Some(0.01065)),
            ]
        );
    }

    #[test]
    fn empty_agent_end_bodies_keep_captured_working_end() {
        // Synthetic: thin agent_end bodies only; the rest is a real capture.
        // Captured: pi 0.73.1, 2026-10-24, same method as the test above.
        let mut ends = 0;
        for line in include_str!("../tests/fixtures/pi_session_cost_0.73.1.jsonl").lines() {
            let envelope: Value = serde_json::from_str(line).unwrap();
            let mut data = envelope["data"].clone();
            if data["event"] != "agent_end" {
                continue;
            }
            data["payload"] = serde_json::json!({});
            let raw = frame(data);
            assert_eq!(
                raw.to_lifecycle_events(),
                [
                    LifecycleEvent::WorkingEnd,
                    LifecycleEvent::ProviderModelChange {
                        provider: Some("atmfake".into()),
                        model: Some("fake-b".into()),
                    },
                ]
            );
            assert_eq!(
                raw.session_id.as_deref(),
                Some("01a16f5b-6c34-7238-80bd-c31c15009fe9")
            );
            assert_eq!(raw.pid, Some(1_613_778));
            assert_eq!(raw.tmux_pane.as_deref(), Some("%0"));
            ends += 1;
        }
        assert_eq!(ends, 2);
    }

    #[test]
    fn captured_new_session_translates() {
        // Captured: pi 0.73.1, 2026-10-24, set up as for
        // captured_tool_run_translates, then `/new` and `/quit` with no
        // prompt. pi loads a new pi-atm instance for the new session: the
        // old instance sent the first two frames, and the new one sent the
        // rest on a second connection.
        let raws: Vec<RawPiEvent> = include_str!("../tests/fixtures/pi_new_session_0.73.1.jsonl")
            .lines()
            .map(|line| {
                let frame: Value = serde_json::from_str(line).expect("fixture line");
                // Both instances snapshot pi's startup model.
                assert_eq!(frame["data"]["model"], model_snapshot("fake-a", 128_000));
                serde_json::from_value(frame["data"].clone()).expect("pi_event data")
            })
            .collect();
        let [start, end_old, start_new, end] = raws.as_slice() else {
            panic!("expected 4 frames, got {}", raws.len());
        };

        // One pid throughout; `/new` gives the session a new id.
        let old_id = Some("01a16f2a-5bf2-71ba-9723-99e2063243a8");
        let new_id = Some("01a16f2a-6e61-7670-87cc-d39f08b46aa4");
        for (raw, id) in [
            (start, old_id),
            (end_old, old_id),
            (start_new, new_id),
            (end, new_id),
        ] {
            assert_eq!(raw.pid, Some(1_455_146));
            assert_eq!(raw.session_id.as_deref(), id);
        }

        assert_eq!(
            start.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );
        assert_eq!(
            end_old.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("new".into())
            })
        );
        assert_eq!(
            start_new.to_lifecycle_event(),
            Some(LifecycleEvent::SessionStart {
                source: Some("new".into())
            })
        );
        assert_eq!(
            end.to_lifecycle_event(),
            Some(LifecycleEvent::SessionEnd {
                reason: Some("quit".into())
            })
        );
    }

    #[test]
    fn captured_startup_model_is_not_lost() {
        // Captured: pi 0.73.1, 2026-10-24, private tmux TUI, pi-atm
        // socket tap and local fake provider (see the crate docs).
        let mut session = atm_core::SessionDomain::new(
            atm_core::SessionId::new("captured"),
            atm_core::AgentType::GeneralPurpose,
            atm_core::Model::Unknown,
        );
        let mut prompt_costs = Vec::new();
        for line in include_str!("../tests/fixtures/pi_session_cost_0.73.1.jsonl").lines() {
            let envelope: Value = serde_json::from_str(line).unwrap();
            let raw = frame(envelope["data"].clone());
            assert_ne!(raw.event, PiEventType::ModelSelect);
            for event in raw.to_lifecycle_events() {
                session.apply_lifecycle_event(&event);
            }
            assert_eq!(atm_core::SessionView::from_domain(&session).model, "fake-b");
            match raw.event {
                PiEventType::SessionStart
                | PiEventType::AgentEnd
                | PiEventType::SessionShutdown => {
                    assert_eq!(session.status, atm_core::SessionStatus::Idle);
                }
                _ => assert_eq!(session.status, atm_core::SessionStatus::Working),
            }
            if raw.event == PiEventType::AgentEnd {
                prompt_costs.push(session.cost.as_microdollars());
            }
        }
        assert_eq!(prompt_costs, [6_975, 10_650]);
    }

    #[test]
    fn complete_mapping_preserves_primary_without_usable_model() {
        // Synthetic: absent/malformed snapshot mutations of startup.
        for model in [
            Value::Null,
            serde_json::json!({"provider": "atmfake"}),
            serde_json::json!([]),
        ] {
            let event = frame(serde_json::json!({
                "event": "session_start", "payload": {"reason": "startup"}, "model": model,
            }));
            assert_eq!(
                event.to_lifecycle_events(),
                [LifecycleEvent::SessionStart {
                    source: Some("startup".into()),
                }]
            );
        }
        assert_eq!(
            raw(PiEventType::AgentEnd, PiPayload::default()).to_lifecycle_events(),
            [LifecycleEvent::WorkingEnd]
        );
    }

    #[test]
    fn complete_mapping_never_revives_shutdown_or_suppressed_events() {
        // Synthetic: snapshots on suppressed and malformed primary events
        // must not create registry rows or evidence by themselves.
        for name in [
            "tool_execution_start",
            "tool_result",
            "message_update",
            "future_event",
            "tool_call",
        ] {
            let event = frame(serde_json::json!({
                "event": name, "model": model_snapshot("fake-b", 32_000),
            }));
            assert!(event.to_lifecycle_events().is_empty(), "{name}");
        }
        let event = frame(serde_json::json!({
            "event": "session_shutdown", "payload": {"reason": "quit"},
            "model": model_snapshot("fake-b", 32_000),
        }));
        assert_eq!(
            event.to_lifecycle_events(),
            [LifecycleEvent::SessionEnd {
                reason: Some("quit".into()),
            }]
        );
    }

    #[test]
    fn complete_mapping_model_select_payload_wins_over_snapshot() {
        // Synthetic: a conflicting envelope must not undo explicit selection.
        let event = frame(serde_json::json!({
            "event": "model_select", "payload": {"model": model_snapshot("fake-b", 32_000)},
            "model": model_snapshot("fake-a", 128_000),
        }));
        assert_eq!(
            event.to_lifecycle_events(),
            [LifecycleEvent::ProviderModelChange {
                provider: Some("atmfake".into()),
                model: Some("fake-b".into()),
            }]
        );
    }

    #[test]
    fn captured_session_cost_translates() {
        // Captured: pi 0.73.1, 2026-10-24, set up as for
        // captured_tool_run_translates, starting on fake-b ($3/$15 per M).
        // Prompt 1 made two priced requests (a read tool call at 0.0033,
        // then a reply at 0.003675) and prompt 2 one (0.003675). pi's
        // footer showed `$0.007`, then `$0.011` and `3.8%/32k`.
        let raws: Vec<RawPiEvent> = include_str!("../tests/fixtures/pi_session_cost_0.73.1.jsonl")
            .lines()
            .map(|line| {
                let frame: Value = serde_json::from_str(line).expect("fixture line");
                assert_eq!(frame["data"]["model"], model_snapshot("fake-b", 32_000));
                serde_json::from_value(frame["data"].clone()).expect("pi_event data")
            })
            .collect();
        let events: Vec<Option<LifecycleEvent>> =
            raws.iter().map(RawPiEvent::to_lifecycle_event).collect();
        let [start, input, working, first_context, tool_start, tool_end, turn_end, context, last_turn_end, working_end, input_2, working_2, context_2, turn_end_2, working_end_2, end] =
            events.as_slice()
        else {
            panic!("expected 16 frames, got {}", events.len());
        };

        let update = |tokens, current_tokens, cost_usd| {
            Some(LifecycleEvent::ContextUpdate {
                tokens,
                current_tokens,
                context_window_size: Some(32_000),
                cost_usd,
            })
        };
        assert_eq!(
            start,
            &Some(LifecycleEvent::SessionStart {
                source: Some("startup".into())
            })
        );
        for input in [input, input_2] {
            assert!(matches!(input, Some(LifecycleEvent::PromptSubmit { .. })));
        }
        for working in [working, working_2] {
            assert_eq!(working, &Some(LifecycleEvent::WorkingStart));
        }
        // Before any reply pi counts the prompt alone: 6 tokens, $0.
        assert_eq!(first_context, &update(Some(0), Some(6), Some(0.0)));
        assert!(matches!(
            tool_start,
            Some(LifecycleEvent::ToolCallStart { .. })
        ));
        assert!(matches!(tool_end, Some(LifecycleEvent::ToolCallEnd { .. })));
        // Each turn_end carries the session's total so far. The active
        // context after the tool call includes pi's estimate for its result.
        assert_eq!(turn_end, &update(Some(1020), Some(1025), Some(0.0033)));
        assert_eq!(context, &update(Some(1020), Some(1025), Some(0.0033)));
        assert_eq!(
            last_turn_end,
            &update(Some(2225), Some(1205), Some(0.006975))
        );
        assert_eq!(context_2, &update(Some(2225), Some(1207), Some(0.006975)));
        assert_eq!(turn_end_2, &update(Some(3430), Some(1205), Some(0.01065)));
        // agent_end still ends each prompt, after its last reply counted.
        for working_end in [working_end, working_end_2] {
            assert_eq!(working_end, &Some(LifecycleEvent::WorkingEnd));
        }
        assert_eq!(
            end,
            &Some(LifecycleEvent::SessionEnd {
                reason: Some("quit".into())
            })
        );

        // Folded into a session, as atmd does: the cost after each prompt
        // is the run's total so far, matching pi's footer.
        let mut session = atm_core::SessionDomain::new(
            atm_core::SessionId::new("captured"),
            atm_core::AgentType::GeneralPurpose,
            atm_core::Model::Unknown,
        );
        let apply = |session: &mut atm_core::SessionDomain, events: &[Option<LifecycleEvent>]| {
            for event in events.iter().flatten() {
                session.apply_lifecycle_event(event);
            }
        };
        apply(&mut session, events.get(..10).expect("prompt 1 frames"));
        assert_eq!(session.cost.as_microdollars(), 6_975);
        apply(&mut session, events.get(10..15).expect("prompt 2 frames"));
        assert_eq!(session.cost.as_microdollars(), 10_650);
        assert_eq!(session.context.format(), "3.8% (1.2K/32K)");
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
