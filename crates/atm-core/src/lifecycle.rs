//! Vendor-neutral agent lifecycle events.
//!
//! `LifecycleEvent` is the abstraction that crosses the daemon boundary —
//! every vendor adapter (Claude Code, pi, future) translates its native
//! event vocabulary into this enum. The daemon's session state machine
//! only ever pattern-matches on `LifecycleEvent`, never on a raw vendor
//! event type.
//!
//! ## Design
//!
//! Three properties drove the shape:
//!
//! 1. **No vendor escape hatch.** Earlier drafts had a `VendorSpecific`
//!    variant; that turned out to leak vendor knowledge into every
//!    consumer. Instead, every event maps to a generic concept (e.g.
//!    Claude `SubagentStart` → `ChildSessionStart`).
//! 2. **`NeedsInput` is adapter-synthesized, not raw.** Pi has no
//!    permission-prompt event — its extension synthesizes one inside a
//!    `tool_call` handler. Claude only signals it via `PreToolUse` of an
//!    interactive tool. So this variant is emitted by the translation
//!    layer, never directly by a vendor.
//! 3. **Provider/model are session metadata, not per-event.** Pi is
//!    provider-agnostic — one session can switch from Anthropic to
//!    OpenAI mid-stream — so changes ride a dedicated
//!    `ProviderModelChange` variant rather than annotating every event.
//!
//! Tool identity rides the typed `Tool` enum instead of free strings,
//! so the well-known set is defined once and the open tail of MCP /
//! vendor-specific names lives in `Tool::Other(String)`.

use crate::Tool;
use serde::{Deserialize, Serialize};

/// Sub-kind of a notification, for the cases the daemon special-cases.
///
/// Pi doesn't use these strings — its permission gating is extension-
/// mediated and surfaces via `NeedsInputReason::PermissionGate`. The
/// known variants here are Claude `Notification(notification_type)`
/// values plus the `setup` synthetic kind we emit when translating
/// Claude's one-time `Setup` hook event.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum NotificationKind {
    /// Claude permission prompt — agent is waiting on a yes/no.
    PermissionPrompt,
    /// Claude MCP elicitation — extension wants structured input.
    ElicitationDialog,
    /// Claude idle prompt — agent has gone idle.
    IdlePrompt,
    /// Claude one-time `Setup` hook (renamed from raw event).
    Setup,
    /// Generic informational notification.
    Info,
    /// Any other notification kind (vendor-specific, future kinds).
    Other(String),
}

impl NotificationKind {
    /// Wire-format string for this kind.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::PermissionPrompt => "permission_prompt",
            Self::ElicitationDialog => "elicitation_dialog",
            Self::IdlePrompt => "idle_prompt",
            Self::Setup => "setup",
            Self::Info => "info",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl NotificationKind {
    /// Canonical lookup for known variants. Returns `None` for inputs
    /// that should fall through to `Other(_)`.
    fn try_from_known(s: &str) -> Option<Self> {
        Some(match s {
            "permission_prompt" => Self::PermissionPrompt,
            "elicitation_dialog" => Self::ElicitationDialog,
            "idle_prompt" => Self::IdlePrompt,
            "setup" => Self::Setup,
            "info" => Self::Info,
            _ => return None,
        })
    }
}

impl From<&str> for NotificationKind {
    fn from(s: &str) -> Self {
        Self::try_from_known(s).unwrap_or_else(|| Self::Other(s.to_string()))
    }
}

impl From<String> for NotificationKind {
    fn from(s: String) -> Self {
        // Reuse the owned String on the Other path to avoid re-allocation.
        Self::try_from_known(&s).unwrap_or(Self::Other(s))
    }
}

impl From<NotificationKind> for String {
    fn from(k: NotificationKind) -> Self {
        match k {
            NotificationKind::Other(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

/// Why a session is awaiting user input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum NeedsInputReason {
    /// Claude `PreToolUse` for an interactive tool
    /// (`AskUserQuestion`, `EnterPlanMode`, `ExitPlanMode`).
    InteractiveTool { tool: Tool },
    /// Pi extension-mediated `tool_call` permission gate.
    PermissionGate { tool: Tool },
    /// Generic notification-driven prompt
    /// (Claude `Notification(permission_prompt|elicitation_dialog)`,
    /// pi `atm_needs_input_open`). `label` carries an optional
    /// vendor-supplied human string (e.g. the dialog title or the
    /// command being gated) so the TUI can show *what* is being
    /// asked, not just that *something* is.
    Notification {
        kind: NotificationKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// Vendor-neutral lifecycle event.
///
/// Adapters translate native vendor events into these variants; the
/// daemon dispatches on them to update session state.
///
/// `Eq` is intentionally not derived: `ContextUpdate` carries an
/// `Option<f64>` cost, which is not `Eq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LifecycleEvent {
    /// Session opened. `source` carries why (Claude: `startup`/`resume`/
    /// `clear`; pi: `startup`).
    SessionStart { source: Option<String> },

    /// Session closed. `reason` is vendor-supplied when available
    /// (pi `session_shutdown.reason`; Claude `SessionEnd.reason`).
    SessionEnd { reason: Option<String> },

    /// Agent began a working block.
    /// (Pi `agent_start`; Claude has no direct analog — synthesized
    /// from `UserPromptSubmit` or first `PreToolUse`.)
    WorkingStart,

    /// Agent finished a working block.
    /// (Pi `agent_end`; Claude `Stop`.)
    WorkingEnd,

    /// Explicit idle signal. Distinct from `WorkingEnd` because pi can
    /// transition idle → idle (no work happened) via
    /// `ctx.isIdle() && !ctx.hasPendingMessages()`.
    Idle,

    /// User submitted a prompt.
    PromptSubmit { prompt: Option<String> },

    /// Session is waiting on user input.
    NeedsInput { reason: NeedsInputReason },

    /// A tool started executing.
    ///
    /// `tool_use_id` is the correlation id pairing this with its later
    /// `ToolCallEnd` (Claude `tool_use_id`, pi `toolCallId`). `input`
    /// is the tool arguments JSON, when the vendor exposes it.
    ToolCallStart {
        name: Tool,
        tool_use_id: Option<String>,
        input: Option<serde_json::Value>,
    },

    /// A tool finished executing. `tool_use_id` matches the originating
    /// `ToolCallStart`.
    ToolCallEnd {
        name: Tool,
        tool_use_id: Option<String>,
        is_error: bool,
    },

    /// Context compaction is starting. `trigger` is the cause
    /// (Claude: `auto`/`manual`).
    ContextCompactStart { trigger: Option<String> },

    /// Periodic context-usage update.
    ///
    /// `tokens` is the cumulative session total. Vendors that expose
    /// the active request/context separately can also provide
    /// `current_tokens` and its `context_window_size`. Every field is
    /// optional because vendors expose different subsets.
    ContextUpdate {
        tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        current_tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window_size: Option<u32>,
        cost_usd: Option<f64>,
    },

    /// Provider or model changed mid-session (pi `model_select`).
    ProviderModelChange {
        provider: Option<String>,
        model: Option<String>,
    },

    /// Free-form notification surfaced to the user. `kind` carries an
    /// optional sub-type the UI can render specially.
    Notification {
        message: Option<String>,
        kind: Option<NotificationKind>,
    },

    /// A child session (subagent / task / fork) started.
    /// `id` is a vendor-supplied correlation id; `role` is a free-form
    /// tag (e.g. Claude subagent role: `"explore"`, `"plan"`).
    ChildSessionStart {
        id: Option<String>,
        role: Option<String>,
    },

    /// A child session finished.
    ChildSessionEnd { id: Option<String> },
}

impl LifecycleEvent {
    /// True if this event ends/clears the session's working state.
    #[must_use]
    pub fn is_terminal_for_turn(&self) -> bool {
        matches!(
            self,
            Self::WorkingEnd | Self::Idle | Self::SessionEnd { .. }
        )
    }

    /// True if this event opens the session's working state.
    #[must_use]
    pub fn is_starting(&self) -> bool {
        matches!(
            self,
            Self::SessionStart { .. } | Self::WorkingStart | Self::PromptSubmit { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_event_serde_roundtrip() {
        let cases = vec![
            LifecycleEvent::SessionStart {
                source: Some("startup".into()),
            },
            LifecycleEvent::SessionEnd {
                reason: Some("quit".into()),
            },
            LifecycleEvent::WorkingStart,
            LifecycleEvent::WorkingEnd,
            LifecycleEvent::Idle,
            LifecycleEvent::PromptSubmit {
                prompt: Some("hello".into()),
            },
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::InteractiveTool {
                    tool: Tool::AskUserQuestion,
                },
            },
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::PermissionGate { tool: Tool::Bash },
            },
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: None,
                },
            },
            LifecycleEvent::NeedsInput {
                reason: NeedsInputReason::Notification {
                    kind: NotificationKind::PermissionPrompt,
                    label: Some("rm -rf /tmp".into()),
                },
            },
            LifecycleEvent::ToolCallStart {
                name: Tool::Bash,
                tool_use_id: Some("tu_123".into()),
                input: Some(serde_json::json!({"command": "ls /tmp"})),
            },
            LifecycleEvent::ToolCallEnd {
                name: Tool::Bash,
                tool_use_id: Some("tu_123".into()),
                is_error: false,
            },
            LifecycleEvent::ContextCompactStart {
                trigger: Some("auto".into()),
            },
            LifecycleEvent::ContextUpdate {
                tokens: Some(1024),
                current_tokens: Some(512),
                context_window_size: Some(200_000),
                cost_usd: Some(0.05),
            },
            LifecycleEvent::ProviderModelChange {
                provider: Some("openai-codex".into()),
                model: Some("gpt-5.5".into()),
            },
            LifecycleEvent::Notification {
                message: Some("hi".into()),
                kind: Some(NotificationKind::Info),
            },
            LifecycleEvent::ChildSessionStart {
                id: Some("agent-1".into()),
                role: Some("explore".into()),
            },
            LifecycleEvent::ChildSessionEnd {
                id: Some("agent-1".into()),
            },
        ];

        for ev in cases {
            let json = serde_json::to_string(&ev).expect("serialize");
            let back: LifecycleEvent = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(ev, back, "roundtrip failed: {json}");
        }
    }

    #[test]
    fn context_update_deserializes_legacy_shape() {
        let event: LifecycleEvent =
            serde_json::from_str(r#"{"type":"context_update","tokens":1024,"cost_usd":null}"#)
                .expect("deserialize legacy context update");

        assert_eq!(
            event,
            LifecycleEvent::ContextUpdate {
                tokens: Some(1024),
                current_tokens: None,
                context_window_size: None,
                cost_usd: None,
            }
        );
    }

    #[test]
    fn terminal_and_starting_classification() {
        assert!(LifecycleEvent::WorkingEnd.is_terminal_for_turn());
        assert!(LifecycleEvent::Idle.is_terminal_for_turn());
        assert!(LifecycleEvent::SessionEnd { reason: None }.is_terminal_for_turn());
        assert!(!LifecycleEvent::WorkingStart.is_terminal_for_turn());

        assert!(LifecycleEvent::SessionStart { source: None }.is_starting());
        assert!(LifecycleEvent::WorkingStart.is_starting());
        assert!(LifecycleEvent::PromptSubmit { prompt: None }.is_starting());
        assert!(!LifecycleEvent::WorkingEnd.is_starting());
    }

    #[test]
    fn notification_kind_wire_format_known() {
        assert_eq!(
            serde_json::to_string(&NotificationKind::PermissionPrompt).unwrap(),
            "\"permission_prompt\""
        );
        assert_eq!(
            serde_json::from_str::<NotificationKind>("\"permission_prompt\"").unwrap(),
            NotificationKind::PermissionPrompt
        );
    }

    #[test]
    fn notification_kind_other_passthrough() {
        let custom = NotificationKind::Other("vendor_specific".into());
        let json = serde_json::to_string(&custom).unwrap();
        assert_eq!(json, "\"vendor_specific\"");
        assert_eq!(
            serde_json::from_str::<NotificationKind>(&json).unwrap(),
            custom
        );
    }
}
