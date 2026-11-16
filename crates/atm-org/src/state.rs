//! In-memory fold. Apply checks references and identity invariants, not policy.

use crate::*;
use atm_core::{Money, SessionId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApplyError {
    #[error(transparent)]
    InvalidId(#[from] IdError),
    #[error("unknown project {0}")]
    UnknownProject(ProjectId),
    #[error("unknown agent {0}")]
    UnknownAgent(AgentId),
    #[error("unknown message {0}")]
    UnknownMessage(MessageId),
    #[error("message {0} already exists")]
    DuplicateMessage(MessageId),
    #[error("unknown task {0}")]
    UnknownTask(TaskId),
    #[error("task {0} already exists")]
    DuplicateTask(TaskId),
    #[error("pane {pane} is held by {holder}")]
    PaneOccupied { pane: String, holder: AgentId },
    #[error("unknown decision {0}")]
    UnknownDecision(DecisionId),
    #[error("decision {0} already exists")]
    DuplicateDecision(DecisionId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Ready,
    Clearing,
    Stuck,
    Down,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub id: AgentId,
    pub role: Option<RoleSpec>,
    pub status: AgentStatus,
    pub pane: Option<String>,
    pub session_id: Option<SessionId>,
    /// Pre-clear binding ignored by session reports until a new binding or reset.
    pub excluded_session_id: Option<SessionId>,
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}
impl Agent {
    fn new(id: AgentId, ts: DateTime<Utc>) -> Self {
        Self {
            id,
            role: None,
            status: AgentStatus::Starting,
            pane: None,
            session_id: None,
            excluded_session_id: None,
            reason: None,
            updated_at: ts,
        }
    }
    fn transition(&mut self, status: AgentStatus, ts: DateTime<Utc>) {
        self.status = status;
        self.updated_at = ts;
        self.reason = None;
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: ProjectId,
    pub root: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryStatus {
    Queued,
    Delivering,
    Delivered,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryAttempt {
    pub attempt: u32,
    pub pane: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryReceipt {
    pub session_id: SessionId,
    pub confirmation: String,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub content: MessageContent,
    pub sent_at: DateTime<Utc>,
    pub status: DeliveryStatus,
    pub read_at: Option<DateTime<Utc>>,
    pub last_attempt: Option<DeliveryAttempt>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub receipt: Option<DeliveryReceipt>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub failure: Option<String>,
    pub failed_at: Option<DateTime<Utc>>,
}

/// Unbounded in-memory state: every message and latest summary per session is
/// retained. No compaction, clock, environment or I/O is involved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrgState {
    projects: BTreeMap<ProjectId, Project>,
    agents: BTreeMap<AgentId, Agent>,
    messages: BTreeMap<MessageId, Message>,
    pub(crate) tasks: BTreeMap<u64, Task>,
    pub(crate) costs: crate::cost::CostLedger,
    summaries: HashMap<SessionId, SessionSummary>,
    decisions: BTreeMap<DecisionId, Decision>,
    pub(crate) owner: OwnerState,
}
impl OrgState {
    /// Choose an unregistered folder slug, then the first free -2, -3, ...
    /// suffix. This does not reserve it: the single writer must register it.
    #[must_use]
    pub fn free_project_id(&self, dir_name: &str) -> ProjectId {
        let base = ProjectId::from_folder(dir_name);
        let mut candidate = base.clone();
        for (index, _) in self.projects.iter().enumerate() {
            if !self.projects.contains_key(&candidate) {
                return candidate;
            }
            candidate = base.suffixed(index);
        }
        // N stored projects cannot occupy all N+1 distinct candidates. If every
        // checked candidate was occupied, this last candidate must be free.
        candidate
    }

    #[must_use]
    pub fn project(&self, id: &ProjectId) -> Option<&Project> {
        self.projects.get(id)
    }
    #[must_use]
    pub fn agent(&self, id: &AgentId) -> Option<&Agent> {
        self.agents.get(id)
    }
    #[must_use]
    pub fn agent_for_pane(&self, pane: &str) -> Option<&Agent> {
        self.agents
            .values()
            .find(|a| a.pane.as_deref() == Some(pane))
    }
    #[must_use]
    pub fn message(&self, id: &MessageId) -> Option<&Message> {
        self.messages.get(id)
    }
    #[must_use]
    pub fn session_summary(&self, id: &SessionId) -> Option<&SessionSummary> {
        self.summaries.get(id)
    }
    pub fn projects(&self) -> impl Iterator<Item = &Project> {
        self.projects.values()
    }
    pub fn agents(&self) -> impl Iterator<Item = &Agent> {
        self.agents.values()
    }
    pub fn messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.values()
    }
    #[must_use]
    pub fn decision(&self, id: &DecisionId) -> Option<&Decision> {
        self.decisions.get(id)
    }
    pub fn decisions(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.values()
    }
    /// All addressed messages, including read/failed ones, ordered by sequence.
    #[must_use]
    pub fn inbox(&self, who: &Address) -> Vec<&Message> {
        self.messages().filter(|m| &m.content.to == who).collect()
    }
    #[must_use]
    pub fn agent_sessions(&self, id: &AgentId) -> Vec<&SessionSummary> {
        let mut sessions: Vec<_> = self
            .summaries
            .values()
            .filter(|s| s.agent_id.as_ref() == Some(id))
            .collect();
        sessions.sort_by(|a, b| {
            a.started
                .cmp(&b.started)
                .then_with(|| a.session_id.as_str().cmp(b.session_id.as_str()))
        });
        sessions
    }
    /// Latest reported cost per session, summed once per session. None means no
    /// reported cost, not zero. Overflow follows core Money's saturating sum.
    #[must_use]
    pub fn agent_cost(&self, id: &AgentId) -> Option<Money> {
        self.agent_sessions(id)
            .iter()
            .filter_map(|s| s.cost)
            .reduce(|a, b| a + b)
    }

    pub(crate) fn require_agent(&self, id: &AgentId) -> Result<&Agent, ApplyError> {
        id.validate()?;
        self.agent(id)
            .ok_or_else(|| ApplyError::UnknownAgent(id.clone()))
    }
    pub(crate) fn require_message(&self, id: &MessageId) -> Result<&Message, ApplyError> {
        self.message(id).ok_or(ApplyError::UnknownMessage(*id))
    }
    pub(crate) fn require_decision(&self, id: &DecisionId) -> Result<&Decision, ApplyError> {
        self.decision(id).ok_or(ApplyError::UnknownDecision(*id))
    }
    pub(crate) fn require_project(&self, id: &ProjectId) -> Result<(), ApplyError> {
        self.project(id)
            .map(|_| ())
            .ok_or_else(|| ApplyError::UnknownProject(id.clone()))
    }
    pub(crate) fn validate_address(&self, address: &Address) -> Result<(), ApplyError> {
        if let Address::Agent(id) = address {
            self.require_agent(id)?;
        }
        Ok(())
    }
    pub(crate) fn available_pane(&self, id: &AgentId, pane: &str) -> Result<(), ApplyError> {
        valid_external_id("pane", pane)?;
        if let Some(holder) = self.agent_for_pane(pane).filter(|a| &a.id != id) {
            return Err(ApplyError::PaneOccupied {
                pane: pane.into(),
                holder: holder.id.clone(),
            });
        }
        Ok(())
    }

    /// Validate every reference before mutation, including whole read batches.
    /// Shared with decide so accepted events satisfy the fold's invariants.
    pub(crate) fn validate_event(&self, event: &OrgEvent) -> Result<(), ApplyError> {
        match event {
            OrgEvent::ProjectRegistered { .. } => {}
            OrgEvent::AgentUpRequested { agent_id, role } => {
                agent_id.validate()?;
                crate::ids::validate_name(&role.name)?;
                self.require_project(&agent_id.project)?;
            }
            OrgEvent::AgentAdopted {
                agent_id,
                pane,
                session_id,
            } => {
                agent_id.validate()?;
                self.require_project(&agent_id.project)?;
                self.available_pane(agent_id, pane)?;
                if let Some(id) = session_id {
                    valid_external_id("session id", id.as_str())?;
                }
            }
            OrgEvent::AgentPaneCreated { agent_id, pane } => {
                self.require_agent(agent_id)?;
                self.available_pane(agent_id, pane)?;
            }
            OrgEvent::AgentSessionBound {
                agent_id,
                session_id,
            } => {
                self.require_agent(agent_id)?;
                valid_external_id("session id", session_id.as_str())?;
            }
            OrgEvent::AgentSessionLost { agent_id }
            | OrgEvent::AgentEditorReady { agent_id }
            | OrgEvent::AgentRevalidationRequested { agent_id }
            | OrgEvent::AgentClearRequested { agent_id }
            | OrgEvent::AgentStuck { agent_id, .. }
            | OrgEvent::AgentDown { agent_id, .. } => {
                self.require_agent(agent_id)?;
            }
            OrgEvent::MessageSent(content) => {
                self.validate_address(&content.from)?;
                self.validate_address(&content.to)?;
            }
            OrgEvent::MessageDeliveryAttempted {
                message_id, pane, ..
            } => {
                self.require_message(message_id)?;
                valid_external_id("pane", pane)?;
            }
            OrgEvent::MessageDelivered {
                message_id,
                session_id,
                ..
            } => {
                self.require_message(message_id)?;
                valid_external_id("session id", session_id.as_str())?;
            }
            OrgEvent::MessageDeliveryFailed { message_id, .. } => {
                self.require_message(message_id)?;
            }
            OrgEvent::MessageRead { message_ids } => {
                for id in message_ids {
                    self.require_message(id)?;
                }
            }
            OrgEvent::TaskCreated(fact) => fact.check(self)?,
            OrgEvent::TaskAssigned(fact) => fact.check(self)?,
            OrgEvent::TaskStateChanged(fact) => fact.check(self)?,
            OrgEvent::TaskSubmitted(fact) => fact.check(self)?,
            OrgEvent::TaskReviewStarted(fact) => fact.check(self)?,
            OrgEvent::TaskVerdict(fact) => fact.check(self)?,
            OrgEvent::TaskAttached(fact) => fact.check(self)?,
            OrgEvent::TaskReminded(fact) => fact.check(self)?,
            OrgEvent::TaskFlagged(fact) => fact.check(self)?,
            OrgEvent::TaskCostRecorded(fact) => fact.check(self)?,
            OrgEvent::TaskUsageObserved(fact) => fact.check(self)?,
            OrgEvent::SessionSummary(summary) => {
                valid_external_id("session id", summary.session_id.as_str())?;
                if let Some(pane) = &summary.pane {
                    valid_external_id("pane", pane)?;
                }
                if let Some(id) = &summary.agent_id {
                    self.require_agent(id)?;
                }
            }
            OrgEvent::ControlSent(_) | OrgEvent::ControlSpawned(_) | OrgEvent::ControlKilled(_) => {
            }
            OrgEvent::DecisionRequested(request) => self.validate_address(&request.requester)?,
            OrgEvent::DecisionTakenProvisionally(ProvisionalChoice {
                decision_id, by, ..
            })
            | OrgEvent::DecisionResolved(DecisionResolutionEvent {
                resolution:
                    DecisionResolution {
                        decision_id, by, ..
                    },
                ..
            })
            | OrgEvent::DecisionOverruled(DecisionResolutionEvent {
                resolution:
                    DecisionResolution {
                        decision_id, by, ..
                    },
                ..
            })
            | OrgEvent::DecisionWithdrawn(DecisionWithdrawal {
                decision_id, by, ..
            }) => {
                self.require_decision(decision_id)?;
                self.validate_address(by)?;
            }
            OrgEvent::DecisionExpired { decision_id } => {
                self.require_decision(decision_id)?;
            }
            OrgEvent::OwnerAway(OwnerAway { by, .. }) | OrgEvent::OwnerBack(OwnerBack { by }) => {
                self.validate_address(by)?;
            }
            OrgEvent::AgentSaveRequested {
                agent_id,
                message_id,
            }
            | OrgEvent::AgentSaved {
                agent_id,
                message_id,
            } => {
                self.require_agent(agent_id)?;
                self.require_message(message_id)?;
            }
        }
        Ok(())
    }

    /// Apply committed facts without enforcing today's transition policy.
    /// Any error leaves the aggregate unchanged.
    pub fn apply(
        &mut self,
        seq: u64,
        ts: DateTime<Utc>,
        event: &OrgEvent,
    ) -> Result<(), ApplyError> {
        self.validate_event(event)?;
        if matches!(event, OrgEvent::MessageSent(_))
            && self.messages.contains_key(&MessageId::new(seq))
        {
            return Err(ApplyError::DuplicateMessage(MessageId::new(seq)));
        }
        if matches!(event, OrgEvent::DecisionRequested(_))
            && self.decisions.contains_key(&DecisionId::new(seq))
        {
            return Err(ApplyError::DuplicateDecision(DecisionId::new(seq)));
        }
        self.track_cost_transition(event, ts);
        match event {
            OrgEvent::ProjectRegistered { project_id, root } => {
                self.projects.insert(
                    project_id.clone(),
                    Project {
                        id: project_id.clone(),
                        root: root.clone(),
                    },
                );
            }
            OrgEvent::AgentUpRequested { agent_id, role } => {
                let mut agent = Agent::new(agent_id.clone(), ts);
                agent.role = Some(role.clone());
                self.agents.insert(agent_id.clone(), agent);
            }
            OrgEvent::AgentAdopted {
                agent_id,
                pane,
                session_id,
            } => {
                let agent = self
                    .agents
                    .entry(agent_id.clone())
                    .or_insert_with(|| Agent::new(agent_id.clone(), ts));
                agent.transition(
                    if session_id.is_some() {
                        AgentStatus::Ready
                    } else {
                        AgentStatus::Starting
                    },
                    ts,
                );
                agent.pane = Some(pane.clone());
                agent.session_id.clone_from(session_id);
                if session_id.is_some() {
                    agent.excluded_session_id = None;
                }
            }
            OrgEvent::AgentPaneCreated { agent_id, pane } => {
                let agent = self.agent_mut(agent_id)?;
                agent.pane = Some(pane.clone());
                agent.updated_at = ts;
            }
            OrgEvent::AgentSessionBound {
                agent_id,
                session_id,
            } => {
                let agent = self.agent_mut(agent_id)?;
                agent.transition(AgentStatus::Ready, ts);
                agent.session_id = Some(session_id.clone());
                agent.excluded_session_id = None;
            }
            OrgEvent::AgentEditorReady { agent_id } => {
                self.agent_mut(agent_id)?.transition(AgentStatus::Ready, ts);
            }
            OrgEvent::AgentRevalidationRequested { agent_id } => {
                self.agent_mut(agent_id)?
                    .transition(AgentStatus::Starting, ts);
            }
            OrgEvent::AgentSessionLost { agent_id }
            | OrgEvent::AgentClearRequested { agent_id } => {
                let agent = self.agent_mut(agent_id)?;
                agent.transition(
                    if matches!(event, OrgEvent::AgentSessionLost { .. }) {
                        AgentStatus::Starting
                    } else {
                        AgentStatus::Clearing
                    },
                    ts,
                );
                if matches!(event, OrgEvent::AgentClearRequested { .. }) {
                    if let Some(session_id) = agent.session_id.take() {
                        agent.excluded_session_id = Some(session_id);
                    }
                }
                agent.session_id = None;
            }
            OrgEvent::AgentStuck { agent_id, reason }
            | OrgEvent::AgentDown { agent_id, reason } => {
                let agent = self.agent_mut(agent_id)?;
                let down = matches!(event, OrgEvent::AgentDown { .. });
                agent.transition(
                    if down {
                        AgentStatus::Down
                    } else {
                        AgentStatus::Stuck
                    },
                    ts,
                );
                agent.reason = Some(reason.clone());
                if down {
                    agent.pane = None;
                    agent.session_id = None;
                    agent.excluded_session_id = None;
                }
            }
            OrgEvent::MessageSent(content) => {
                let owner = content.to == Address::Owner;
                let id = MessageId::new(seq);
                self.messages.insert(
                    id,
                    Message {
                        id,
                        content: content.clone(),
                        sent_at: ts,
                        status: if owner {
                            DeliveryStatus::Delivered
                        } else {
                            DeliveryStatus::Queued
                        },
                        read_at: None,
                        last_attempt: None,
                        last_attempt_at: None,
                        receipt: None,
                        delivered_at: owner.then_some(ts),
                        failure: None,
                        failed_at: None,
                    },
                );
            }
            OrgEvent::MessageDeliveryAttempted {
                message_id,
                attempt,
                pane,
            } => {
                let message = self.message_mut(message_id)?;
                message.status = DeliveryStatus::Delivering;
                message.last_attempt = Some(DeliveryAttempt {
                    attempt: *attempt,
                    pane: pane.clone(),
                });
                message.last_attempt_at = Some(ts);
            }
            OrgEvent::MessageDelivered {
                message_id,
                session_id,
                confirmation,
            } => {
                let message = self.message_mut(message_id)?;
                message.status = DeliveryStatus::Delivered;
                message.receipt = Some(DeliveryReceipt {
                    session_id: session_id.clone(),
                    confirmation: confirmation.clone(),
                });
                message.delivered_at = Some(ts);
            }
            OrgEvent::MessageDeliveryFailed {
                message_id,
                reason,
                final_failure,
            } => {
                let message = self.message_mut(message_id)?;
                message.status = if *final_failure {
                    DeliveryStatus::Failed
                } else {
                    DeliveryStatus::Queued
                };
                message.failure = Some(reason.clone());
                message.failed_at = Some(ts);
            }
            OrgEvent::MessageRead { message_ids } => {
                // All IDs were checked before any mutation. Repeated IDs/read
                // events preserve the first read timestamp.
                for id in message_ids {
                    if let Some(message) = self.messages.get_mut(id) {
                        message.read_at.get_or_insert(ts);
                    }
                }
            }
            OrgEvent::TaskCreated(fact) => fact.fold(&mut self.tasks, seq, ts)?,
            OrgEvent::TaskAssigned(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskStateChanged(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskSubmitted(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskReviewStarted(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskVerdict(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskAttached(fact) => fact.fold(self.task_mut(&fact.task_id)?),
            OrgEvent::TaskReminded(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskFlagged(fact) => fact.fold(self.task_mut(&fact.task_id)?, ts),
            OrgEvent::TaskCostRecorded(fact) => fact.fold(self.task_mut(&fact.task_id)?),
            OrgEvent::TaskUsageObserved(fact) => self.apply_usage(fact),
            OrgEvent::SessionSummary(summary) => {
                self.summaries
                    .insert(summary.session_id.clone(), summary.clone());
            }
            OrgEvent::ControlSent(_)
            | OrgEvent::ControlSpawned(_)
            | OrgEvent::ControlKilled(_)
            | OrgEvent::AgentSaveRequested { .. }
            | OrgEvent::AgentSaved { .. } => {}
            OrgEvent::DecisionRequested(request) => {
                let id = DecisionId::new(seq);
                self.decisions
                    .insert(id, Decision::new(id, request.clone(), ts));
            }
            OrgEvent::DecisionTakenProvisionally(choice) => {
                let decision = self.decision_mut(&choice.decision_id)?;
                decision.status = DecisionStatus::TakenProvisionally;
                decision.provisional = Some(choice.clone());
                decision.taken_at = Some(ts);
            }
            OrgEvent::DecisionResolved(answer) | OrgEvent::DecisionOverruled(answer) => {
                let resolution = &answer.resolution;
                let decision = self.decision_mut(&resolution.decision_id)?;
                decision.close(
                    if matches!(event, OrgEvent::DecisionResolved(_)) {
                        DecisionStatus::Resolved
                    } else {
                        DecisionStatus::Overruled
                    },
                    ts,
                );
                decision.resolution = Some(resolution.clone());
            }
            OrgEvent::DecisionExpired { decision_id } => {
                self.decision_mut(decision_id)?
                    .close(DecisionStatus::Expired, ts);
            }
            OrgEvent::DecisionWithdrawn(withdrawal) => {
                let decision = self.decision_mut(&withdrawal.decision_id)?;
                decision.close(DecisionStatus::Withdrawn, ts);
                decision.withdrawal = Some(withdrawal.clone());
            }
            OrgEvent::OwnerAway(fact) => fact.fold(&mut self.owner, ts),
            OrgEvent::OwnerBack(_) => self.owner.away_since = None,
        }
        Ok(())
    }
    fn decision_mut(&mut self, id: &DecisionId) -> Result<&mut Decision, ApplyError> {
        self.decisions
            .get_mut(id)
            .ok_or(ApplyError::UnknownDecision(*id))
    }
    fn agent_mut(&mut self, id: &AgentId) -> Result<&mut Agent, ApplyError> {
        self.agents
            .get_mut(id)
            .ok_or_else(|| ApplyError::UnknownAgent(id.clone()))
    }
    fn message_mut(&mut self, id: &MessageId) -> Result<&mut Message, ApplyError> {
        self.messages
            .get_mut(id)
            .ok_or(ApplyError::UnknownMessage(*id))
    }
}

pub(crate) fn valid_external_id(kind: &'static str, value: &str) -> Result<(), IdError> {
    if value.is_empty() {
        Err(IdError {
            kind,
            value: value.into(),
            rule: "must not be empty",
        })
    } else {
        Ok(())
    }
}
