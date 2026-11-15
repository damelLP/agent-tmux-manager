//! Command policy. Reports are commands too: decide, append, then apply.

use crate::*;
use atm_core::SessionId;
use chrono::{DateTime, Utc};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgCommand {
    RegisterProject {
        project_id: ProjectId,
        root: String,
        /// The daemon verified the old root no longer holds this project.
        move_root: bool,
    },
    Up {
        agent_id: AgentId,
        role: RoleSpec,
    },
    Adopt {
        agent_id: AgentId,
        pane: String,
        session_id: Option<SessionId>,
    },
    Clear {
        agent_id: AgentId,
    },
    Down {
        agent_id: AgentId,
        reason: String,
    },
    Respawn {
        agent_id: AgentId,
        role: RoleSpec,
        reason: String,
    },
    PaneCreated {
        agent_id: AgentId,
        pane: String,
    },
    SessionBound {
        agent_id: AgentId,
        session_id: SessionId,
    },
    SessionLost {
        agent_id: AgentId,
    },
    /// Suspend readiness during restart/adopt revalidation, retaining the pane,
    /// recorded session and exclusion. Observe and report under the same agent
    /// reservation. Ready transitions to Starting; Starting with a recorded
    /// session is already revalidating. Other source states refuse.
    Revalidate {
        agent_id: AgentId,
    },
    /// The caller must observe the fresh editor after the operation this report
    /// completes (clear, up or respawn), or a safe idle editor during restart/
    /// adopt revalidation, and submit it in the same serialized step owning the
    /// agent. An observation queued across a later clear or respawn can complete
    /// the wrong readiness: this command carries no pane
    /// or generation to reject it. See the crate's command-policy limits.
    EditorReady {
        agent_id: AgentId,
    },
    Stuck {
        agent_id: AgentId,
        reason: String,
    },
    SendMessage(MessageContent),
    DeliveryAttempted {
        message_id: MessageId,
        attempt: u32,
        pane: String,
    },
    Delivered {
        message_id: MessageId,
        session_id: SessionId,
        confirmation: String,
    },
    DeliveryFailed {
        message_id: MessageId,
        reason: String,
        final_failure: bool,
    },
    ReadMessages {
        who: Address,
        message_ids: Vec<MessageId>,
    },
    Task(TaskCommand),
    RecordSummary(SessionSummary),
    ControlSent(ControlAudit),
    ControlSpawned(ControlAudit),
    ControlKilled(ControlAudit),
    Decision(DecisionCommand),
    /// The away switch, and the away table applied to one pending decision.
    Owner(OwnerCommand),
    /// Audit: a handover asked the agent to save with this message.
    SaveRequested {
        agent_id: AgentId,
        message_id: MessageId,
    },
    /// Audit: the agent's save turn ended with its memory file changed.
    Saved {
        agent_id: AgentId,
        message_id: MessageId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Refusal {
    #[error(transparent)]
    Invariant(#[from] ApplyError),
    #[error("project {project_id} is registered at {registered:?}, not {requested:?}")]
    ProjectRootConflict {
        project_id: ProjectId,
        registered: String,
        requested: String,
    },
    #[error("cannot {operation} agent {agent_id} while {state:?}")]
    InvalidAgentState {
        agent_id: AgentId,
        state: AgentStatus,
        operation: &'static str,
    },
    #[error("agent {agent_id} already holds pane {pane}")]
    AgentAlreadyBound { agent_id: AgentId, pane: String },
    #[error("role name {role_name:?} differs from agent {agent_id}")]
    RoleNameMismatch {
        agent_id: AgentId,
        role_name: String,
    },
    #[error("cannot {operation} message {message_id} while {state:?}")]
    InvalidDeliveryState {
        message_id: MessageId,
        state: DeliveryStatus,
        operation: &'static str,
    },
    #[error("message {message_id} is not addressed to {who}")]
    WrongRecipient { message_id: MessageId, who: Address },
    #[error("invalid recipient {0}")]
    InvalidRecipient(Address),
    #[error(transparent)]
    Task(#[from] TaskRefusal),
    #[error("invalid {field}: {reason}")]
    InvalidValue { field: &'static str, reason: String },
    #[error(transparent)]
    Decision(#[from] DecisionRefusal),
    #[error(transparent)]
    Away(#[from] AwayRefusal),
}

impl OrgCommand {
    // The candidate shares validation with apply before policy can emit it.
    fn event(&self) -> OrgEvent {
        match self.clone() {
            Self::RegisterProject {
                project_id, root, ..
            } => OrgEvent::ProjectRegistered { project_id, root },
            Self::Up { agent_id, role } | Self::Respawn { agent_id, role, .. } => {
                OrgEvent::AgentUpRequested { agent_id, role }
            }
            Self::Adopt {
                agent_id,
                pane,
                session_id,
            } => OrgEvent::AgentAdopted {
                agent_id,
                pane,
                session_id,
            },
            Self::Clear { agent_id } => OrgEvent::AgentClearRequested { agent_id },
            Self::Down { agent_id, reason } => OrgEvent::AgentDown { agent_id, reason },
            Self::PaneCreated { agent_id, pane } => OrgEvent::AgentPaneCreated { agent_id, pane },
            Self::SessionBound {
                agent_id,
                session_id,
            } => OrgEvent::AgentSessionBound {
                agent_id,
                session_id,
            },
            Self::SessionLost { agent_id } => OrgEvent::AgentSessionLost { agent_id },
            Self::EditorReady { agent_id } => OrgEvent::AgentEditorReady { agent_id },
            Self::Revalidate { agent_id } => OrgEvent::AgentRevalidationRequested { agent_id },
            Self::Stuck { agent_id, reason } => OrgEvent::AgentStuck { agent_id, reason },
            Self::SendMessage(content) => OrgEvent::MessageSent(content),
            Self::DeliveryAttempted {
                message_id,
                attempt,
                pane,
            } => OrgEvent::MessageDeliveryAttempted {
                message_id,
                attempt,
                pane,
            },
            Self::Delivered {
                message_id,
                session_id,
                confirmation,
            } => OrgEvent::MessageDelivered {
                message_id,
                session_id,
                confirmation,
            },
            Self::DeliveryFailed {
                message_id,
                reason,
                final_failure,
            } => OrgEvent::MessageDeliveryFailed {
                message_id,
                reason,
                final_failure,
            },
            Self::ReadMessages { message_ids, .. } => OrgEvent::MessageRead { message_ids },
            Self::Task(command) => command.event(),
            Self::RecordSummary(summary) => OrgEvent::SessionSummary(summary),
            Self::ControlSent(audit) => OrgEvent::ControlSent(audit),
            Self::ControlSpawned(audit) => OrgEvent::ControlSpawned(audit),
            Self::ControlKilled(audit) => OrgEvent::ControlKilled(audit),
            Self::Decision(command) => command.event(),
            Self::Owner(command) => command.event(),
            Self::SaveRequested {
                agent_id,
                message_id,
            } => OrgEvent::AgentSaveRequested {
                agent_id,
                message_id,
            },
            Self::Saved {
                agent_id,
                message_id,
            } => OrgEvent::AgentSaved {
                agent_id,
                message_id,
            },
        }
    }
}

impl OrgState {
    /// Validate intent without changing state or assigning event sequences.
    /// Event timestamps come from the log writer. `now` is compared only with a
    /// decision's deadline; no other policy needs a clock.
    pub fn decide(
        &self,
        command: &OrgCommand,
        now: DateTime<Utc>,
    ) -> Result<Vec<OrgEvent>, Refusal> {
        let event = command.event();
        self.validate_event(&event)?;
        let changed = match command {
            OrgCommand::RegisterProject {
                project_id,
                root,
                move_root,
            } => {
                nonempty("root", root)?;
                if let Some(project) = self.project(project_id) {
                    if &project.root != root && !move_root {
                        return Err(Refusal::ProjectRootConflict {
                            project_id: project_id.clone(),
                            registered: project.root.clone(),
                            requested: root.clone(),
                        });
                    }
                }
                self.project(project_id).is_none_or(|p| &p.root != root)
            }
            OrgCommand::Up { agent_id, role } => {
                matching_role(agent_id, role)?;
                if let Some(agent) = self.agent(agent_id) {
                    if matches!(
                        agent.status,
                        AgentStatus::Starting | AgentStatus::Ready | AgentStatus::Clearing
                    ) || (agent.status == AgentStatus::Stuck && agent.pane.is_some())
                    {
                        return Err(agent_refusal(agent, "up"));
                    }
                }
                true
            }
            OrgCommand::Adopt {
                agent_id,
                pane,
                session_id,
            } => {
                if let Some(agent) = self.agent(agent_id) {
                    same_pane_or_unbound(agent, pane)?;
                    let status = if session_id.is_some() {
                        AgentStatus::Ready
                    } else {
                        AgentStatus::Starting
                    };
                    agent.pane.as_ref() != Some(pane)
                        || &agent.session_id != session_id
                        || agent.status != status
                        || (session_id.is_some() && agent.excluded_session_id.is_some())
                } else {
                    true
                }
            }
            OrgCommand::PaneCreated { agent_id, pane } => {
                let agent = self.require_agent(agent_id)?;
                same_pane_or_unbound(agent, pane)?;
                if agent.pane.as_ref() == Some(pane) {
                    false
                } else {
                    if agent.status != AgentStatus::Starting {
                        return Err(agent_refusal(agent, "record pane"));
                    }
                    true
                }
            }
            OrgCommand::SessionBound {
                agent_id,
                session_id,
            } => {
                let agent = self.require_agent(agent_id)?;
                if agent.status == AgentStatus::Down || agent.pane.is_none() {
                    return Err(agent_refusal(agent, "bind session"));
                }
                agent.excluded_session_id.as_ref() != Some(session_id)
                    && (agent.status != AgentStatus::Ready
                        || agent.session_id.as_ref() != Some(session_id))
            }
            OrgCommand::EditorReady { agent_id } => {
                let agent = self.require_agent(agent_id)?;
                if agent.status == AgentStatus::Down || agent.pane.is_none() {
                    return Err(agent_refusal(agent, "record editor ready"));
                }
                agent.status != AgentStatus::Ready
            }
            OrgCommand::Revalidate { agent_id } => {
                let agent = self.require_agent(agent_id)?;
                match agent.status {
                    AgentStatus::Ready => true,
                    AgentStatus::Starting if agent.session_id.is_some() => false,
                    _ => return Err(agent_refusal(agent, "revalidate")),
                }
            }
            OrgCommand::SessionLost { agent_id } => {
                let agent = self.require_agent(agent_id)?;
                !matches!(agent.status, AgentStatus::Clearing | AgentStatus::Down)
                    && !(matches!(
                        agent.status,
                        AgentStatus::Starting | AgentStatus::Stuck | AgentStatus::Ready
                    ) && agent.session_id.is_none())
            }
            OrgCommand::Clear { agent_id } => {
                let agent = self.require_agent(agent_id)?;
                if agent.status != AgentStatus::Ready {
                    return Err(agent_refusal(agent, "clear"));
                }
                true
            }
            OrgCommand::Stuck { agent_id, reason } => {
                let agent = self.require_agent(agent_id)?;
                if agent.status == AgentStatus::Down {
                    return Err(agent_refusal(agent, "mark stuck"));
                }
                agent.status != AgentStatus::Stuck || agent.reason.as_ref() != Some(reason)
            }
            OrgCommand::Down { agent_id, .. } => {
                self.require_agent(agent_id)?.status != AgentStatus::Down
            }
            OrgCommand::Respawn {
                agent_id,
                role,
                reason,
            } => {
                matching_role(agent_id, role)?;
                if self.require_agent(agent_id)?.status != AgentStatus::Down {
                    return Ok(vec![
                        OrgEvent::AgentDown {
                            agent_id: agent_id.clone(),
                            reason: reason.clone(),
                        },
                        event,
                    ]);
                }
                true
            }
            OrgCommand::SendMessage(content) => {
                if content.to == Address::System {
                    return Err(Refusal::InvalidRecipient(Address::System));
                }
                true
            }
            OrgCommand::DeliveryAttempted {
                message_id,
                attempt,
                pane,
            } => {
                let message = self.require_message(message_id)?;
                let same = message
                    .last_attempt
                    .as_ref()
                    .is_some_and(|a| a.attempt == *attempt && &a.pane == pane);
                if terminal(message) || (message.status == DeliveryStatus::Delivering && same) {
                    false
                } else {
                    if message.status != DeliveryStatus::Queued {
                        return Err(delivery_refusal(message, "attempt delivery"));
                    }
                    if *attempt == 0
                        || message
                            .last_attempt
                            .as_ref()
                            .is_some_and(|a| *attempt <= a.attempt)
                    {
                        return Err(Refusal::InvalidValue {
                            field: "attempt",
                            reason: "must be positive and increase".into(),
                        });
                    }
                    true
                }
            }
            OrgCommand::Delivered { message_id, .. } => {
                self.require_message(message_id)?.status != DeliveryStatus::Delivered
            }
            OrgCommand::DeliveryFailed {
                message_id,
                reason,
                final_failure,
            } => {
                let message = self.require_message(message_id)?;
                if terminal(message)
                    || (!final_failure
                        && message.status == DeliveryStatus::Queued
                        && message.failure.as_ref() == Some(reason))
                {
                    false
                } else {
                    if message.status != DeliveryStatus::Delivering
                        && !(message.status == DeliveryStatus::Queued && *final_failure)
                    {
                        return Err(delivery_refusal(message, "fail delivery"));
                    }
                    true
                }
            }
            OrgCommand::ReadMessages { who, message_ids } => {
                self.validate_address(who)?;
                let mut unread = false;
                for id in message_ids {
                    let message = self.require_message(id)?;
                    if &message.content.to != who {
                        return Err(Refusal::WrongRecipient {
                            message_id: *id,
                            who: who.clone(),
                        });
                    }
                    if message.status != DeliveryStatus::Delivered {
                        return Err(delivery_refusal(message, "read"));
                    }
                    unread |= message.read_at.is_none();
                }
                unread
            }
            OrgCommand::Task(command) => return self.decide_task(command, event),
            OrgCommand::RecordSummary(summary) => {
                self.session_summary(&summary.session_id) != Some(summary)
            }
            OrgCommand::ControlSent(audit)
            | OrgCommand::ControlSpawned(audit)
            | OrgCommand::ControlKilled(audit) => {
                nonempty("pane", &audit.pane)?;
                true
            }
            OrgCommand::Decision(command) => return self.decide_decision(command, event, now),
            OrgCommand::Owner(command) => return self.decide_owner(command, event, now),
            OrgCommand::SaveRequested {
                agent_id,
                message_id,
            }
            | OrgCommand::Saved {
                agent_id,
                message_id,
            } => {
                self.require_agent(agent_id)?;
                self.require_message(message_id)?;
                true
            }
        };
        Ok(if changed { vec![event] } else { vec![] })
    }
}

fn matching_role(id: &AgentId, role: &RoleSpec) -> Result<(), Refusal> {
    if role.name != id.name {
        Err(Refusal::RoleNameMismatch {
            agent_id: id.clone(),
            role_name: role.name.clone(),
        })
    } else {
        Ok(())
    }
}
fn same_pane_or_unbound(agent: &Agent, pane: &str) -> Result<(), Refusal> {
    if let Some(held) = &agent.pane {
        if held != pane {
            return Err(Refusal::AgentAlreadyBound {
                agent_id: agent.id.clone(),
                pane: held.clone(),
            });
        }
    }
    Ok(())
}
fn agent_refusal(agent: &Agent, operation: &'static str) -> Refusal {
    Refusal::InvalidAgentState {
        agent_id: agent.id.clone(),
        state: agent.status,
        operation,
    }
}
fn delivery_refusal(message: &Message, operation: &'static str) -> Refusal {
    Refusal::InvalidDeliveryState {
        message_id: message.id,
        state: message.status,
        operation,
    }
}
fn terminal(message: &Message) -> bool {
    matches!(
        message.status,
        DeliveryStatus::Delivered | DeliveryStatus::Failed
    )
}
pub(crate) fn nonempty(field: &'static str, value: &str) -> Result<(), Refusal> {
    if value.is_empty() {
        Err(Refusal::InvalidValue {
            field,
            reason: "must not be empty".into(),
        })
    } else {
        Ok(())
    }
}
