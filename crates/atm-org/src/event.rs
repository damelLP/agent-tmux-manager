//! Durable event bodies. IDs are strings; optional facts emit null. Role config
//! is flat, timestamps are RFC3339, and summary cost is an exact microdollar
//! object (not core Money's floating-point USD encoding). Known JSON bodies
//! tolerate additive fields; unknown kinds are skipped before decoding a body.

use crate::{Address, AgentId, MessageId, ProjectId, RoleSpec, TaskId};
use atm_core::{Harness, Money, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrgError {
    #[error(transparent)]
    Apply(#[from] crate::ApplyError),
    #[error("replay counter overflow")]
    CounterOverflow,
    #[error("invalid body for {kind}: {source}")]
    Body {
        kind: String,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Note,
    Brief,
    Report,
    Question,
    Answer,
    Review,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageContent {
    pub from: Address,
    pub to: Address,
    pub kind: MessageKind,
    pub body: String,
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub pane: Option<String>,
    pub agent_id: Option<AgentId>,
    pub harness: Harness,
    pub model: Option<String>,
    #[serde(default, with = "optional_money")]
    pub cost: Option<Money>,
    pub first_prompt: Option<String>,
    pub started: DateTime<Utc>,
    pub ended: Option<DateTime<Utc>>,
}

mod optional_money {
    use super::*;
    #[derive(Serialize, Deserialize)]
    struct Amount {
        microdollars: i64,
    }
    pub fn serialize<S: serde::Serializer>(
        value: &Option<Money>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .map(|m| Amount {
                microdollars: m.as_microdollars(),
            })
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Money>, D::Error> {
        Ok(Option::<Amount>::deserialize(deserializer)?
            .map(|m| Money::from_microdollars(m.microdollars)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlAudit {
    pub pane: String,
    pub action: Value,
}

/// Events contain facts, not transition policy. Their body fields are the log
/// format: newer readers must continue to accept older bodies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body")]
pub enum OrgEvent {
    #[serde(rename = "project.registered")]
    ProjectRegistered { project_id: ProjectId, root: String },
    #[serde(rename = "agent.up_requested")]
    AgentUpRequested { agent_id: AgentId, role: RoleSpec },
    #[serde(rename = "agent.pane_created")]
    AgentPaneCreated { agent_id: AgentId, pane: String },
    #[serde(rename = "agent.adopted")]
    AgentAdopted {
        agent_id: AgentId,
        pane: String,
        session_id: Option<SessionId>,
    },
    #[serde(rename = "agent.session_bound")]
    AgentSessionBound {
        agent_id: AgentId,
        session_id: SessionId,
    },
    #[serde(rename = "agent.session_lost")]
    AgentSessionLost { agent_id: AgentId },
    #[serde(rename = "agent.clear_requested")]
    AgentClearRequested { agent_id: AgentId },
    #[serde(rename = "agent.stuck")]
    AgentStuck { agent_id: AgentId, reason: String },
    #[serde(rename = "agent.down")]
    AgentDown { agent_id: AgentId, reason: String },
    #[serde(rename = "msg.sent")]
    MessageSent(MessageContent),
    #[serde(rename = "msg.delivery_attempted")]
    MessageDeliveryAttempted {
        message_id: MessageId,
        attempt: u32,
        pane: String,
    },
    #[serde(rename = "msg.delivered")]
    MessageDelivered {
        message_id: MessageId,
        session_id: SessionId,
        confirmation: String,
    },
    #[serde(rename = "msg.delivery_failed")]
    MessageDeliveryFailed {
        message_id: MessageId,
        reason: String,
        #[serde(rename = "final")]
        final_failure: bool,
    },
    #[serde(rename = "msg.read")]
    MessageRead { message_ids: Vec<MessageId> },
    #[serde(rename = "session.summary")]
    SessionSummary(SessionSummary),
    #[serde(rename = "control.sent")]
    ControlSent(ControlAudit),
    #[serde(rename = "control.spawned")]
    ControlSpawned(ControlAudit),
    #[serde(rename = "control.killed")]
    ControlKilled(ControlAudit),
}

impl OrgEvent {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProjectRegistered { .. } => "project.registered",
            Self::AgentUpRequested { .. } => "agent.up_requested",
            Self::AgentPaneCreated { .. } => "agent.pane_created",
            Self::AgentAdopted { .. } => "agent.adopted",
            Self::AgentSessionBound { .. } => "agent.session_bound",
            Self::AgentSessionLost { .. } => "agent.session_lost",
            Self::AgentClearRequested { .. } => "agent.clear_requested",
            Self::AgentStuck { .. } => "agent.stuck",
            Self::AgentDown { .. } => "agent.down",
            Self::MessageSent(_) => "msg.sent",
            Self::MessageDeliveryAttempted { .. } => "msg.delivery_attempted",
            Self::MessageDelivered { .. } => "msg.delivered",
            Self::MessageDeliveryFailed { .. } => "msg.delivery_failed",
            Self::MessageRead { .. } => "msg.read",
            Self::SessionSummary(_) => "session.summary",
            Self::ControlSent(_) => "control.sent",
            Self::ControlSpawned(_) => "control.spawned",
            Self::ControlKilled(_) => "control.killed",
        }
    }

    #[must_use]
    pub fn to_body(&self) -> Value {
        // Every value below has infallible JSON serialization: strings, integers,
        // enums, timestamps, arrays and structs, with no non-string map keys or
        // fallible user serializers. Money uses the integer codec above.
        match self {
            Self::ProjectRegistered { project_id, root } => {
                json!({"project_id":project_id,"root":root})
            }
            Self::AgentUpRequested { agent_id, role } => json!({"agent_id":agent_id,"role":role}),
            Self::AgentPaneCreated { agent_id, pane } => json!({"agent_id":agent_id,"pane":pane}),
            Self::AgentAdopted {
                agent_id,
                pane,
                session_id,
            } => json!({"agent_id":agent_id,"pane":pane,"session_id":session_id}),
            Self::AgentSessionBound {
                agent_id,
                session_id,
            } => json!({"agent_id":agent_id,"session_id":session_id}),
            Self::AgentSessionLost { agent_id } | Self::AgentClearRequested { agent_id } => {
                json!({"agent_id":agent_id})
            }
            Self::AgentStuck { agent_id, reason } | Self::AgentDown { agent_id, reason } => {
                json!({"agent_id":agent_id,"reason":reason})
            }
            Self::MessageSent(content) => json!(content),
            Self::MessageDeliveryAttempted {
                message_id,
                attempt,
                pane,
            } => json!({"message_id":message_id,"attempt":attempt,"pane":pane}),
            Self::MessageDelivered {
                message_id,
                session_id,
                confirmation,
            } => {
                json!({"message_id":message_id,"session_id":session_id,"confirmation":confirmation})
            }
            Self::MessageDeliveryFailed {
                message_id,
                reason,
                final_failure,
            } => json!({"message_id":message_id,"reason":reason,"final":final_failure}),
            Self::MessageRead { message_ids } => json!({"message_ids":message_ids}),
            Self::SessionSummary(summary) => json!(summary),
            Self::ControlSent(audit) | Self::ControlSpawned(audit) | Self::ControlKilled(audit) => {
                json!(audit)
            }
        }
    }

    pub fn from_parts(kind: &str, body: &Value) -> Result<Option<Self>, OrgError> {
        if !matches!(
            kind,
            "project.registered"
                | "agent.up_requested"
                | "agent.pane_created"
                | "agent.adopted"
                | "agent.session_bound"
                | "agent.session_lost"
                | "agent.clear_requested"
                | "agent.stuck"
                | "agent.down"
                | "msg.sent"
                | "msg.delivery_attempted"
                | "msg.delivered"
                | "msg.delivery_failed"
                | "msg.read"
                | "session.summary"
                | "control.sent"
                | "control.spawned"
                | "control.killed"
        ) {
            return Ok(None);
        }
        serde_json::from_value(json!({"kind":kind, "body":body}))
            .map(Some)
            .map_err(|source| OrgError::Body {
                kind: kind.into(),
                source,
            })
    }
}
