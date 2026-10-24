//! pi event-name vocabulary.
//!
//! Sourced from pi's `dist/core/extensions/types.d.ts`. At the
//! `agent-tmux-manager-9dn` spike (2026-05-02, pi version not recorded)
//! two events, `tool_call` and `tool_result`, were undeclared there but
//! confirmed real by the trace under `extensions/pi-spike/`.
//!
//! Captured: pi 0.73.1, 2026-10-24, the `on(event: ...)` names in that
//! file. It declares 29 events, `tool_call` and `tool_result` included.
//! All but `thinking_level_select` are variants here; that one lands
//! in `Other`, and pi-atm doesn't forward it.

use serde::{Deserialize, Serialize};
use std::fmt;

/// All pi event types pi is known to emit.
///
/// Wire format: lowercase snake_case strings (`session_start`,
/// `tool_execution_end`, …) — distinct from Claude's PascalCase.
///
/// `Other(String)` is the open-set escape hatch for events pi may add
/// in the future before this enum catches up.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub enum PiEventType {
    // === Session lifecycle ===
    SessionStart,
    SessionBeforeSwitch,
    SessionBeforeFork,
    SessionBeforeCompact,
    SessionCompact,
    SessionShutdown,
    SessionBeforeTree,
    SessionTree,

    // === Resources ===
    ResourcesDiscover,

    // === Conversation context (high-frequency) ===
    Context,
    BeforeProviderRequest,
    AfterProviderResponse,

    // === Agent / turn ===
    BeforeAgentStart,
    AgentStart,
    AgentEnd,
    TurnStart,
    TurnEnd,
    MessageStart,
    MessageUpdate,
    MessageEnd,

    // === Tools ===
    ToolExecutionStart,
    ToolExecutionUpdate,
    ToolExecutionEnd,
    /// Undeclared in pi's type union at the spike, declared in pi
    /// 0.73.1 — fires alongside `tool_execution_start`. Permission-gate
    /// hook point.
    ToolCall,
    /// Undeclared in pi's type union at the spike, declared in pi
    /// 0.73.1 — fires alongside `tool_execution_end`.
    ToolResult,

    // === Misc ===
    ModelSelect,
    UserBash,
    Input,

    // === Synthetic events emitted by `pi-atm` ===
    //
    // Pi has no event for "ctx.ui.select dialog opened" — the only way
    // a passive observer can detect that pi is awaiting user permission
    // is if our extension instruments `ctx.ui.select` itself. When that
    // happens the extension emits these synthetic events with the
    // wire `event` field set to the strings below. They are not
    // produced by pi itself; they're our adapter's contract with the
    // hook script.
    /// `atm_needs_input_open` — `ctx.ui.select` was just called by some
    /// extension (typically pi-amplike's permission gate). Pi is
    /// blocked awaiting the user's response.
    AtmNeedsInputOpen,
    /// `atm_needs_input_resolved` — the dialog closed. Pi resumes work.
    AtmNeedsInputResolved,

    /// Any event name pi emits that this enum doesn't yet recognize.
    Other(String),
}

impl PiEventType {
    /// Canonical wire-format string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::SessionStart => "session_start",
            Self::SessionBeforeSwitch => "session_before_switch",
            Self::SessionBeforeFork => "session_before_fork",
            Self::SessionBeforeCompact => "session_before_compact",
            Self::SessionCompact => "session_compact",
            Self::SessionShutdown => "session_shutdown",
            Self::SessionBeforeTree => "session_before_tree",
            Self::SessionTree => "session_tree",
            Self::ResourcesDiscover => "resources_discover",
            Self::Context => "context",
            Self::BeforeProviderRequest => "before_provider_request",
            Self::AfterProviderResponse => "after_provider_response",
            Self::BeforeAgentStart => "before_agent_start",
            Self::AgentStart => "agent_start",
            Self::AgentEnd => "agent_end",
            Self::TurnStart => "turn_start",
            Self::TurnEnd => "turn_end",
            Self::MessageStart => "message_start",
            Self::MessageUpdate => "message_update",
            Self::MessageEnd => "message_end",
            Self::ToolExecutionStart => "tool_execution_start",
            Self::ToolExecutionUpdate => "tool_execution_update",
            Self::ToolExecutionEnd => "tool_execution_end",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ModelSelect => "model_select",
            Self::UserBash => "user_bash",
            Self::Input => "input",
            Self::AtmNeedsInputOpen => "atm_needs_input_open",
            Self::AtmNeedsInputResolved => "atm_needs_input_resolved",
            Self::Other(s) => s.as_str(),
        }
    }
}

impl fmt::Display for PiEventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PiEventType {
    /// Canonical lookup for known variants. Returns `None` for event
    /// names that should fall through to `Other(_)`.
    fn try_from_known(s: &str) -> Option<Self> {
        Some(match s {
            "session_start" => Self::SessionStart,
            "session_before_switch" => Self::SessionBeforeSwitch,
            "session_before_fork" => Self::SessionBeforeFork,
            "session_before_compact" => Self::SessionBeforeCompact,
            "session_compact" => Self::SessionCompact,
            "session_shutdown" => Self::SessionShutdown,
            "session_before_tree" => Self::SessionBeforeTree,
            "session_tree" => Self::SessionTree,
            "resources_discover" => Self::ResourcesDiscover,
            "context" => Self::Context,
            "before_provider_request" => Self::BeforeProviderRequest,
            "after_provider_response" => Self::AfterProviderResponse,
            "before_agent_start" => Self::BeforeAgentStart,
            "agent_start" => Self::AgentStart,
            "agent_end" => Self::AgentEnd,
            "turn_start" => Self::TurnStart,
            "turn_end" => Self::TurnEnd,
            "message_start" => Self::MessageStart,
            "message_update" => Self::MessageUpdate,
            "message_end" => Self::MessageEnd,
            "tool_execution_start" => Self::ToolExecutionStart,
            "tool_execution_update" => Self::ToolExecutionUpdate,
            "tool_execution_end" => Self::ToolExecutionEnd,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            "model_select" => Self::ModelSelect,
            "user_bash" => Self::UserBash,
            "input" => Self::Input,
            "atm_needs_input_open" => Self::AtmNeedsInputOpen,
            "atm_needs_input_resolved" => Self::AtmNeedsInputResolved,
            _ => return None,
        })
    }
}

impl From<&str> for PiEventType {
    fn from(s: &str) -> Self {
        Self::try_from_known(s).unwrap_or_else(|| Self::Other(s.to_string()))
    }
}

impl From<String> for PiEventType {
    fn from(s: String) -> Self {
        // Reuse the owned String on the Other path to avoid re-allocation.
        Self::try_from_known(&s).unwrap_or(Self::Other(s))
    }
}

impl From<PiEventType> for String {
    fn from(t: PiEventType) -> Self {
        match t {
            PiEventType::Other(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_events_roundtrip() {
        for variant in [
            PiEventType::SessionStart,
            PiEventType::SessionShutdown,
            PiEventType::AgentStart,
            PiEventType::AgentEnd,
            PiEventType::ToolExecutionStart,
            PiEventType::ToolExecutionEnd,
            PiEventType::ToolCall,
            PiEventType::ToolResult,
            PiEventType::Context,
            PiEventType::ModelSelect,
            PiEventType::Input,
        ] {
            let s = variant.as_str().to_string();
            assert_eq!(PiEventType::from(s), variant);
        }
    }

    #[test]
    fn unknown_event_lands_in_other() {
        assert_eq!(
            PiEventType::from("hypothetical_future_event"),
            PiEventType::Other("hypothetical_future_event".to_string())
        );
    }

    #[test]
    fn serde_roundtrips_as_bare_string() {
        assert_eq!(
            serde_json::to_string(&PiEventType::AgentStart).unwrap(),
            "\"agent_start\""
        );
        assert_eq!(
            serde_json::from_str::<PiEventType>("\"tool_call\"").unwrap(),
            PiEventType::ToolCall
        );
    }
}
