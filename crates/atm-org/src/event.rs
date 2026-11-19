//! Durable event bodies. IDs are strings; optional facts emit null except the
//! omitted-when-absent `msg.sent.delivery_notice` metadata. Role config
//! is flat, timestamps are RFC3339, and summary cost is an exact microdollar
//! object (not core Money's floating-point USD encoding). Known JSON bodies
//! tolerate additive fields; unknown kinds are skipped before decoding a body.
//!
//! `agent.editor_ready` is an additive kind with body `{ "agent_id": "p/lead" }`.
//! It records readiness without altering the session, pane or exclusion. The
//! pre-clear exclusion is derived while folding `agent.clear_requested`, before
//! removing its session; no existing event body changed. Newer readers accept
//! older logs, including committed session bindings today's policy would ignore.
//! Older readers skip editor-ready facts and may remain clearing, starting or
//! stuck: they cannot reconstruct the newer semantics. Compatibility is newer reads older,
//! not a guarantee of safe downgrade or equivalent state in older readers.
//!
//! `agent.revalidation_requested` is additive with the same agent-ID-only body.
//! It records Starting while retaining pane, session and exclusion. Existing
//! kinds are unchanged. Older readers skip it and may incorrectly retain Ready;
//! this is another reason downgrading a newer log is not a safety guarantee.
//!
//! `msg.sent.delivery_notice` optionally records `{source_message_id, reason}`
//! on a system note. Its lifetime key is independent of the note's audience:
//! normally the owner, or a registered agent for a routable reason on ordinary
//! mail to a different named seat. Absent/null metadata reads as None and is
//! omitted on write, preserving older bodies byte-for-byte. Readers that only
//! accept owner-addressed keys must be upgraded before any routed note is written.
//! Pre-key readers ignore the field and cannot preserve deduplication. Unkeyed
//! notes do not claim keys and may be followed by one keyed note after upgrade.

use crate::{
    Address, AgentId, DecisionId, DecisionRequest, DecisionResolutionEvent, DecisionWithdrawal,
    MessageId, ProjectId, ProvisionalChoice, RoleSpec, TaskId,
};
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

/// Lifetime delivery-notice key. Reasons are stable semantic tags, not
/// diagnostics; future nonempty tags remain readable without a codec change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryNotice {
    pub source_message_id: MessageId,
    pub reason: String,
}

impl DeliveryNotice {
    /// Whether this reason may be routed to a manager. The organisation fold
    /// separately validates the notice's source and both addresses.
    #[must_use]
    pub fn may_route_to_manager(&self) -> bool {
        matches!(self.reason.as_str(), "unobserved" | "evidence_unavailable")
            || matches!(self.reason.split_once('.'), Some(("blocked" | "hold", cause)) if !cause.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageContent {
    pub from: Address,
    pub to: Address,
    pub kind: MessageKind,
    pub body: String,
    pub attachments: Vec<String>,
    pub task: Option<TaskId>,
    /// The note and its durable dedupe key are one committed `msg.sent` fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_notice: Option<DeliveryNotice>,
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

pub(crate) mod optional_money {
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
    #[serde(rename = "agent.editor_ready")]
    AgentEditorReady { agent_id: AgentId },
    #[serde(rename = "agent.revalidation_requested")]
    AgentRevalidationRequested { agent_id: AgentId },
    #[serde(rename = "agent.clear_requested")]
    AgentClearRequested { agent_id: AgentId },
    #[serde(rename = "agent.stuck")]
    AgentStuck { agent_id: AgentId, reason: String },
    #[serde(rename = "agent.down")]
    AgentDown { agent_id: AgentId, reason: String },
    /// Audit of a handover: the agent was asked, by this message, to save
    /// its state. Changes no state.
    #[serde(rename = "agent.save_requested")]
    AgentSaveRequested {
        agent_id: AgentId,
        message_id: MessageId,
    },
    /// Audit of a handover: the agent's save turn ended and its memory file
    /// had changed. Changes no state.
    #[serde(rename = "agent.saved")]
    AgentSaved {
        agent_id: AgentId,
        message_id: MessageId,
    },
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
    #[serde(rename = "task.created")]
    TaskCreated(crate::TaskCreated),
    #[serde(rename = "task.assigned")]
    TaskAssigned(crate::TaskAssigned),
    #[serde(rename = "task.state_changed")]
    TaskStateChanged(crate::TaskStateChanged),
    #[serde(rename = "task.submitted")]
    TaskSubmitted(crate::TaskSubmitted),
    #[serde(rename = "task.review_started")]
    TaskReviewStarted(crate::TaskReviewStarted),
    #[serde(rename = "task.verdict")]
    TaskVerdict(crate::TaskVerdictGiven),
    #[serde(rename = "task.attached")]
    TaskAttached(crate::TaskAttached),
    #[serde(rename = "task.reminded")]
    TaskReminded(crate::TaskReminded),
    #[serde(rename = "task.flagged")]
    TaskFlagged(crate::TaskFlagged),
    #[serde(rename = "task.cost_recorded")]
    TaskCostRecorded(crate::TaskCostRecorded),
    #[serde(rename = "task.usage_observed")]
    TaskUsageObserved(crate::TaskUsageObserved),
    #[serde(rename = "session.summary")]
    SessionSummary(SessionSummary),
    #[serde(rename = "control.sent")]
    ControlSent(ControlAudit),
    #[serde(rename = "control.spawned")]
    ControlSpawned(ControlAudit),
    #[serde(rename = "control.killed")]
    ControlKilled(ControlAudit),
    #[serde(rename = "decision.requested")]
    DecisionRequested(DecisionRequest),
    #[serde(rename = "decision.taken_provisionally")]
    DecisionTakenProvisionally(ProvisionalChoice),
    #[serde(rename = "decision.resolved")]
    DecisionResolved(DecisionResolutionEvent),
    #[serde(rename = "decision.overruled")]
    DecisionOverruled(DecisionResolutionEvent),
    #[serde(rename = "decision.expired")]
    DecisionExpired { decision_id: DecisionId },
    #[serde(rename = "decision.withdrawn")]
    DecisionWithdrawn(DecisionWithdrawal),
    #[serde(rename = "owner.away")]
    OwnerAway(crate::OwnerAway),
    #[serde(rename = "owner.back")]
    OwnerBack(crate::OwnerBack),
    #[serde(rename = "owner.relay_set")]
    OwnerRelaySet(crate::OwnerRelaySet),
    #[serde(rename = "owner.digest_created")]
    OwnerDigestCreated(crate::OwnerDigestCreated),
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
            Self::AgentEditorReady { .. } => "agent.editor_ready",
            Self::AgentRevalidationRequested { .. } => "agent.revalidation_requested",
            Self::AgentClearRequested { .. } => "agent.clear_requested",
            Self::AgentStuck { .. } => "agent.stuck",
            Self::AgentDown { .. } => "agent.down",
            Self::AgentSaveRequested { .. } => "agent.save_requested",
            Self::AgentSaved { .. } => "agent.saved",
            Self::MessageSent(_) => "msg.sent",
            Self::MessageDeliveryAttempted { .. } => "msg.delivery_attempted",
            Self::MessageDelivered { .. } => "msg.delivered",
            Self::MessageDeliveryFailed { .. } => "msg.delivery_failed",
            Self::MessageRead { .. } => "msg.read",
            Self::TaskCreated(_) => "task.created",
            Self::TaskAssigned(_) => "task.assigned",
            Self::TaskStateChanged(_) => "task.state_changed",
            Self::TaskSubmitted(_) => "task.submitted",
            Self::TaskReviewStarted(_) => "task.review_started",
            Self::TaskVerdict(_) => "task.verdict",
            Self::TaskAttached(_) => "task.attached",
            Self::TaskReminded(_) => "task.reminded",
            Self::TaskFlagged(_) => "task.flagged",
            Self::TaskCostRecorded(_) => "task.cost_recorded",
            Self::TaskUsageObserved(_) => "task.usage_observed",
            Self::SessionSummary(_) => "session.summary",
            Self::ControlSent(_) => "control.sent",
            Self::ControlSpawned(_) => "control.spawned",
            Self::ControlKilled(_) => "control.killed",
            Self::DecisionRequested(_) => "decision.requested",
            Self::DecisionTakenProvisionally(_) => "decision.taken_provisionally",
            Self::DecisionResolved(_) => "decision.resolved",
            Self::DecisionOverruled(_) => "decision.overruled",
            Self::DecisionExpired { .. } => "decision.expired",
            Self::DecisionWithdrawn(_) => "decision.withdrawn",
            Self::OwnerAway(_) => "owner.away",
            Self::OwnerBack(_) => "owner.back",
            Self::OwnerRelaySet(_) => "owner.relay_set",
            Self::OwnerDigestCreated(_) => "owner.digest_created",
        }
    }

    #[must_use]
    pub fn to_body(&self) -> Value {
        // Serialization cannot fail for these strings, integers, enums,
        // timestamps, arrays and structs (no fallible serializers or non-string
        // map keys; Money uses the exact integer codec). Every variant has a
        // body in the adjacent-tag derive. Null is only a defensive fallback;
        // all-variant object/roundtrip tests prove the normal path is taken.
        serde_json::to_value(self)
            .ok()
            .and_then(|mut tagged| tagged.as_object_mut()?.remove("body"))
            .unwrap_or(Value::Null)
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
                | "agent.editor_ready"
                | "agent.revalidation_requested"
                | "agent.clear_requested"
                | "agent.stuck"
                | "agent.down"
                | "agent.save_requested"
                | "agent.saved"
                | "msg.sent"
                | "msg.delivery_attempted"
                | "msg.delivered"
                | "msg.delivery_failed"
                | "msg.read"
                | "task.created"
                | "task.assigned"
                | "task.state_changed"
                | "task.submitted"
                | "task.review_started"
                | "task.verdict"
                | "task.attached"
                | "task.reminded"
                | "task.flagged"
                | "task.cost_recorded"
                | "task.usage_observed"
                | "session.summary"
                | "control.sent"
                | "control.spawned"
                | "control.killed"
                | "decision.requested"
                | "decision.taken_provisionally"
                | "decision.resolved"
                | "decision.overruled"
                | "decision.expired"
                | "decision.withdrawn"
                | "owner.away"
                | "owner.back"
                | "owner.relay_set"
                | "owner.digest_created"
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
